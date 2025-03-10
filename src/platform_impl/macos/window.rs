use std::{
    collections::VecDeque,
    convert::TryInto,
    f64, ops,
    os::raw::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use raw_window_handle::{
    AppKitDisplayHandle, AppKitWindowHandle, RawDisplayHandle, RawWindowHandle,
};

use crate::{
    dpi::{
        LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, Position, Size, Size::Logical,
    },
    error::{ExternalError, NotSupportedError, OsError as RootOsError},
    icon::Icon,
    monitor::{MonitorHandle as RootMonitorHandle, VideoMode as RootVideoMode},
    platform::macos::WindowExtMacOS,
    platform_impl::platform::{
        app_state::AppState,
        ffi,
        monitor::{self, MonitorHandle, VideoMode},
        util::{self, IdRef},
        view::{self, new_view, ViewState},
        window_delegate::new_delegate,
        OsError,
    },
    window::{
        CursorGrabMode, CursorIcon, Fullscreen, UserAttentionType, WindowAttributes,
        WindowId as RootWindowId,
    },
};
use cocoa::{
    appkit::{
        self, CGFloat, NSApp, NSApplication, NSApplicationPresentationOptions, NSColor,
        NSRequestUserAttentionType, NSScreen, NSView, NSWindow, NSWindowButton, NSWindowStyleMask,
    },
    base::{id, nil},
    foundation::{NSDictionary, NSPoint, NSRect, NSSize},
};
use core_graphics::display::{CGDisplay, CGDisplayMode};
use objc2::foundation::{is_main_thread, NSObject, NSUInteger};
use objc2::rc::autoreleasepool;
use objc2::runtime::{Bool, Object};
use objc2::{declare_class, ClassType};

use super::appkit::{NSCursor, NSResponder, NSWindow as NSWindowClass};

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub usize);

impl WindowId {
    pub const unsafe fn dummy() -> Self {
        Self(0)
    }
}

impl From<WindowId> for u64 {
    fn from(window_id: WindowId) -> Self {
        window_id.0 as u64
    }
}

impl From<u64> for WindowId {
    fn from(raw_id: u64) -> Self {
        Self(raw_id as usize)
    }
}

// Convert the `cocoa::base::id` associated with a window to a usize to use as a unique identifier
// for the window.
pub fn get_window_id(window_cocoa_id: id) -> WindowId {
    WindowId(window_cocoa_id as *const Object as _)
}

#[derive(Clone)]
pub struct PlatformSpecificWindowBuilderAttributes {
    pub movable_by_window_background: bool,
    pub titlebar_transparent: bool,
    pub title_hidden: bool,
    pub titlebar_hidden: bool,
    pub titlebar_buttons_hidden: bool,
    pub fullsize_content_view: bool,
    pub resize_increments: Option<LogicalSize<f64>>,
    pub disallow_hidpi: bool,
    pub has_shadow: bool,
}

impl Default for PlatformSpecificWindowBuilderAttributes {
    #[inline]
    fn default() -> Self {
        Self {
            movable_by_window_background: false,
            titlebar_transparent: false,
            title_hidden: false,
            titlebar_hidden: false,
            titlebar_buttons_hidden: false,
            fullsize_content_view: false,
            resize_increments: None,
            disallow_hidpi: false,
            has_shadow: true,
        }
    }
}

unsafe fn create_view(
    ns_window: id,
    pl_attribs: &PlatformSpecificWindowBuilderAttributes,
) -> Option<IdRef> {
    let ns_view = new_view(ns_window);
    ns_view.non_nil().map(|ns_view| {
        // The default value of `setWantsBestResolutionOpenGLSurface:` was `false` until
        // macos 10.14 and `true` after 10.15, we should set it to `YES` or `NO` to avoid
        // always the default system value in favour of the user's code
        if !pl_attribs.disallow_hidpi {
            ns_view.setWantsBestResolutionOpenGLSurface_(Bool::YES.as_raw());
        } else {
            ns_view.setWantsBestResolutionOpenGLSurface_(Bool::NO.as_raw());
        }

        // On Mojave, views automatically become layer-backed shortly after being added to
        // a window. Changing the layer-backedness of a view breaks the association between
        // the view and its associated OpenGL context. To work around this, on Mojave we
        // explicitly make the view layer-backed up front so that AppKit doesn't do it
        // itself and break the association with its context.
        if f64::floor(appkit::NSAppKitVersionNumber) > appkit::NSAppKitVersionNumber10_12 {
            ns_view.setWantsLayer(Bool::YES.as_raw());
        }

        ns_view
    })
}

fn create_window(
    attrs: &WindowAttributes,
    pl_attrs: &PlatformSpecificWindowBuilderAttributes,
) -> Option<IdRef> {
    autoreleasepool(|_| unsafe {
        let screen = match attrs.fullscreen {
            Some(Fullscreen::Borderless(Some(RootMonitorHandle { inner: ref monitor })))
            | Some(Fullscreen::Exclusive(RootVideoMode {
                video_mode: VideoMode { ref monitor, .. },
            })) => {
                let monitor_screen = monitor.ns_screen();
                Some(monitor_screen.unwrap_or_else(|| appkit::NSScreen::mainScreen(nil)))
            }
            Some(Fullscreen::Borderless(None)) => Some(appkit::NSScreen::mainScreen(nil)),
            None => None,
        };
        let frame = match screen {
            Some(screen) => NSScreen::frame(screen),
            None => {
                let screen = NSScreen::mainScreen(nil);
                let scale_factor = NSScreen::backingScaleFactor(screen) as f64;
                let (width, height) = match attrs.inner_size {
                    Some(size) => {
                        let logical = size.to_logical(scale_factor);
                        (logical.width, logical.height)
                    }
                    None => (800.0, 600.0),
                };
                let (left, bottom) = match attrs.position {
                    Some(position) => {
                        let logical = util::window_position(position.to_logical(scale_factor));
                        // macOS wants the position of the bottom left corner,
                        // but caller is setting the position of top left corner
                        (logical.x, logical.y - height)
                    }
                    // This value is ignored by calling win.center() below
                    None => (0.0, 0.0),
                };
                NSRect::new(NSPoint::new(left, bottom), NSSize::new(width, height))
            }
        };

        let mut masks = if (!attrs.decorations && screen.is_none()) || pl_attrs.titlebar_hidden {
            // Resizable UnownedWindow without a titlebar or borders
            // if decorations is set to false, ignore pl_attrs
            //
            // if the titlebar is hidden, ignore other pl_attrs
            NSWindowStyleMask::NSBorderlessWindowMask
                | NSWindowStyleMask::NSResizableWindowMask
                | NSWindowStyleMask::NSMiniaturizableWindowMask
        } else {
            // default case, resizable window with titlebar and titlebar buttons
            NSWindowStyleMask::NSClosableWindowMask
                | NSWindowStyleMask::NSMiniaturizableWindowMask
                | NSWindowStyleMask::NSResizableWindowMask
                | NSWindowStyleMask::NSTitledWindowMask
        };

        if !attrs.resizable {
            masks &= !NSWindowStyleMask::NSResizableWindowMask;
        }

        if pl_attrs.fullsize_content_view {
            masks |= NSWindowStyleMask::NSFullSizeContentViewWindowMask;
        }

        let ns_window: id = msg_send![WinitWindow::class(), alloc];
        let ns_window = IdRef::new(ns_window.initWithContentRect_styleMask_backing_defer_(
            frame,
            masks,
            appkit::NSBackingStoreBuffered,
            Bool::NO.as_raw(),
        ));

        ns_window.non_nil().map(|ns_window| {
            let title = util::ns_string_id_ref(&attrs.title);
            ns_window.setReleasedWhenClosed_(Bool::NO.as_raw());
            ns_window.setTitle_(*title);
            ns_window.setAcceptsMouseMovedEvents_(Bool::YES.as_raw());

            if pl_attrs.titlebar_transparent {
                ns_window.setTitlebarAppearsTransparent_(Bool::YES.as_raw());
            }
            if pl_attrs.title_hidden {
                ns_window.setTitleVisibility_(appkit::NSWindowTitleVisibility::NSWindowTitleHidden);
            }
            if pl_attrs.titlebar_buttons_hidden {
                for titlebar_button in &[
                    NSWindowButton::NSWindowFullScreenButton,
                    NSWindowButton::NSWindowMiniaturizeButton,
                    NSWindowButton::NSWindowCloseButton,
                    NSWindowButton::NSWindowZoomButton,
                ] {
                    let button = ns_window.standardWindowButton_(*titlebar_button);
                    let _: () = msg_send![button, setHidden: true];
                }
            }
            if pl_attrs.movable_by_window_background {
                ns_window.setMovableByWindowBackground_(Bool::YES.as_raw());
            }

            if attrs.always_on_top {
                let _: () = msg_send![*ns_window, setLevel: ffi::kCGFloatingWindowLevelKey];
            }

            if let Some(increments) = pl_attrs.resize_increments {
                let (w, h) = (increments.width, increments.height);
                if w >= 1.0 && h >= 1.0 {
                    let size = NSSize::new(w as CGFloat, h as CGFloat);
                    // It was concluded (#2411) that there is never a use-case for
                    // "outer" resize increments, hence we set "inner" ones here.
                    // ("outer" in macOS being just resizeIncrements, and "inner" - contentResizeIncrements)
                    // This is consistent with X11 size hints behavior
                    ns_window.setContentResizeIncrements_(size);
                }
            }

            if !pl_attrs.has_shadow {
                ns_window.setHasShadow_(Bool::NO.as_raw());
            }
            if attrs.position.is_none() {
                ns_window.center();
            }
            ns_window
        })
    })
}

declare_class!(
    struct WinitWindow {}

    unsafe impl ClassType for WinitWindow {
        #[inherits(NSResponder, NSObject)]
        type Super = NSWindowClass;
    }

    unsafe impl WinitWindow {
        #[sel(canBecomeMainWindow)]
        fn can_become_main_window(&self) -> bool {
            trace_scope!("canBecomeMainWindow");
            true
        }

        #[sel(canBecomeKeyWindow)]
        fn can_become_key_window(&self) -> bool {
            trace_scope!("canBecomeKeyWindow");
            true
        }
    }
);

#[derive(Default)]
pub struct SharedState {
    pub resizable: bool,
    pub fullscreen: Option<Fullscreen>,
    // This is true between windowWillEnterFullScreen and windowDidEnterFullScreen
    // or windowWillExitFullScreen and windowDidExitFullScreen.
    // We must not toggle fullscreen when this is true.
    pub in_fullscreen_transition: bool,
    // If it is attempted to toggle fullscreen when in_fullscreen_transition is true,
    // Set target_fullscreen and do after fullscreen transition is end.
    pub target_fullscreen: Option<Option<Fullscreen>>,
    pub maximized: bool,
    pub standard_frame: Option<NSRect>,
    is_simple_fullscreen: bool,
    pub saved_style: Option<NSWindowStyleMask>,
    /// Presentation options saved before entering `set_simple_fullscreen`, and
    /// restored upon exiting it. Also used when transitioning from Borderless to
    /// Exclusive fullscreen in `set_fullscreen` because we need to disable the menu
    /// bar in exclusive fullscreen but want to restore the original options when
    /// transitioning back to borderless fullscreen.
    save_presentation_opts: Option<NSApplicationPresentationOptions>,
    pub saved_desktop_display_mode: Option<(CGDisplay, CGDisplayMode)>,
}

impl SharedState {
    pub fn saved_standard_frame(&self) -> NSRect {
        self.standard_frame
            .unwrap_or_else(|| NSRect::new(NSPoint::new(50.0, 50.0), NSSize::new(800.0, 600.0)))
    }
}

impl From<WindowAttributes> for SharedState {
    fn from(attribs: WindowAttributes) -> Self {
        SharedState {
            resizable: attribs.resizable,
            // This fullscreen field tracks the current state of the window
            // (as seen by `WindowDelegate`), and since the window hasn't
            // actually been fullscreened yet, we can't set it yet. This is
            // necessary for state transitions to work right, since otherwise
            // the initial value and the first `set_fullscreen` call would be
            // identical, resulting in a no-op.
            fullscreen: None,
            maximized: attribs.maximized,
            ..Default::default()
        }
    }
}

pub(crate) struct SharedStateMutexGuard<'a> {
    guard: MutexGuard<'a, SharedState>,
    called_from_fn: &'static str,
}

impl<'a> SharedStateMutexGuard<'a> {
    #[inline]
    pub(crate) fn new(guard: MutexGuard<'a, SharedState>, called_from_fn: &'static str) -> Self {
        trace!("Locked shared state in `{}`", called_from_fn);
        Self {
            guard,
            called_from_fn,
        }
    }
}

impl ops::Deref for SharedStateMutexGuard<'_> {
    type Target = SharedState;
    #[inline]
    fn deref(&self) -> &Self::Target {
        self.guard.deref()
    }
}

impl ops::DerefMut for SharedStateMutexGuard<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.deref_mut()
    }
}

impl Drop for SharedStateMutexGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        trace!("Unlocked shared state in `{}`", self.called_from_fn);
    }
}

pub struct UnownedWindow {
    pub ns_window: IdRef, // never changes
    pub ns_view: IdRef,   // never changes
    shared_state: Arc<Mutex<SharedState>>,
    decorations: AtomicBool,
    pub inner_rect: Option<PhysicalSize<u32>>,
}

unsafe impl Send for UnownedWindow {}
unsafe impl Sync for UnownedWindow {}

impl UnownedWindow {
    pub(crate) fn new(
        mut win_attribs: WindowAttributes,
        pl_attribs: PlatformSpecificWindowBuilderAttributes,
    ) -> Result<(Arc<Self>, IdRef), RootOsError> {
        if !is_main_thread() {
            panic!("Windows can only be created on the main thread on macOS");
        }
        trace!("Creating new window");

        let ns_window = create_window(&win_attribs, &pl_attribs)
            .ok_or_else(|| os_error!(OsError::CreationError("Couldn't create `NSWindow`")))?;

        let ns_view = unsafe { create_view(*ns_window, &pl_attribs) }
            .ok_or_else(|| os_error!(OsError::CreationError("Couldn't create `NSView`")))?;

        // Configure the new view as the "key view" for the window
        unsafe {
            ns_window.setContentView_(*ns_view);
            ns_window.setInitialFirstResponder_(*ns_view);
        }

        let scale_factor = unsafe { NSWindow::backingScaleFactor(*ns_window) as f64 };

        unsafe {
            if win_attribs.transparent {
                ns_window.setOpaque_(Bool::NO.as_raw());
                ns_window.setBackgroundColor_(NSColor::clearColor(nil));
            }

            if let Some(dim) = win_attribs.min_inner_size {
                let logical_dim = dim.to_logical(scale_factor);
                set_min_inner_size(*ns_window, logical_dim);
            }
            if let Some(dim) = win_attribs.max_inner_size {
                let logical_dim = dim.to_logical(scale_factor);
                set_max_inner_size(*ns_window, logical_dim);
            }

            use cocoa::foundation::NSArray;
            // register for drag and drop operations.
            let _: () = msg_send![
                *ns_window,
                registerForDraggedTypes:
                    NSArray::arrayWithObject(nil, appkit::NSFilenamesPboardType)
            ];
        }

        // Since `win_attribs` is put into a mutex below, we'll just copy these
        // attributes now instead of bothering to lock it later.
        // Also, `SharedState` doesn't carry `fullscreen` over; it's set
        // indirectly by us calling `set_fullscreen` below, causing handlers in
        // `WindowDelegate` to update the state.
        let fullscreen = win_attribs.fullscreen.take();
        let maximized = win_attribs.maximized;
        let visible = win_attribs.visible;
        let decorations = win_attribs.decorations;
        let inner_rect = win_attribs
            .inner_size
            .map(|size| size.to_physical(scale_factor));

        let window = Arc::new(UnownedWindow {
            ns_view,
            ns_window,
            shared_state: Arc::new(Mutex::new(win_attribs.into())),
            decorations: AtomicBool::new(decorations),
            inner_rect,
        });

        let delegate = new_delegate(&window, fullscreen.is_some());

        // Set fullscreen mode after we setup everything
        window.set_fullscreen(fullscreen);

        // Setting the window as key has to happen *after* we set the fullscreen
        // state, since otherwise we'll briefly see the window at normal size
        // before it transitions.
        if visible {
            // Tightly linked with `app_state::window_activation_hack`
            unsafe { window.ns_window.makeKeyAndOrderFront_(nil) };
        }

        if maximized {
            window.set_maximized(maximized);
        }
        trace!("Done unowned window::new");
        Ok((window, delegate))
    }

    #[track_caller]
    pub(crate) fn lock_shared_state(
        &self,
        called_from_fn: &'static str,
    ) -> SharedStateMutexGuard<'_> {
        SharedStateMutexGuard::new(self.shared_state.lock().unwrap(), called_from_fn)
    }

    fn set_style_mask_async(&self, mask: NSWindowStyleMask) {
        unsafe { util::set_style_mask_async(*self.ns_window, *self.ns_view, mask) };
    }

    fn set_style_mask_sync(&self, mask: NSWindowStyleMask) {
        unsafe { util::set_style_mask_sync(*self.ns_window, *self.ns_view, mask) };
    }

    pub fn id(&self) -> WindowId {
        get_window_id(*self.ns_window)
    }

    pub fn set_title(&self, title: &str) {
        unsafe {
            util::set_title_async(*self.ns_window, title.to_string());
        }
    }

    pub fn set_visible(&self, visible: bool) {
        match visible {
            true => unsafe { util::make_key_and_order_front_async(*self.ns_window) },
            false => unsafe { util::order_out_async(*self.ns_window) },
        }
    }

    #[inline]
    pub fn is_visible(&self) -> Option<bool> {
        let is_visible = unsafe { msg_send![*self.ns_window, isVisible] };
        Some(is_visible)
    }

    pub fn request_redraw(&self) {
        AppState::queue_redraw(RootWindowId(self.id()));
    }

    pub fn outer_position(&self) -> Result<PhysicalPosition<i32>, NotSupportedError> {
        let frame_rect = unsafe { NSWindow::frame(*self.ns_window) };
        let position = LogicalPosition::new(
            frame_rect.origin.x as f64,
            util::bottom_left_to_top_left(frame_rect),
        );
        let scale_factor = self.scale_factor();
        Ok(position.to_physical(scale_factor))
    }

    pub fn inner_position(&self) -> Result<PhysicalPosition<i32>, NotSupportedError> {
        let content_rect = unsafe {
            NSWindow::contentRectForFrameRect_(*self.ns_window, NSWindow::frame(*self.ns_window))
        };
        let position = LogicalPosition::new(
            content_rect.origin.x as f64,
            util::bottom_left_to_top_left(content_rect),
        );
        let scale_factor = self.scale_factor();
        Ok(position.to_physical(scale_factor))
    }

    pub fn set_outer_position(&self, position: Position) {
        let scale_factor = self.scale_factor();
        let position = position.to_logical(scale_factor);
        unsafe {
            util::set_frame_top_left_point_async(*self.ns_window, util::window_position(position));
        }
    }

    #[inline]
    pub fn inner_size(&self) -> PhysicalSize<u32> {
        let view_frame = unsafe { NSView::frame(*self.ns_view) };
        let logical: LogicalSize<f64> =
            (view_frame.size.width as f64, view_frame.size.height as f64).into();
        let scale_factor = self.scale_factor();
        logical.to_physical(scale_factor)
    }

    #[inline]
    pub fn outer_size(&self) -> PhysicalSize<u32> {
        let view_frame = unsafe { NSWindow::frame(*self.ns_window) };
        let logical: LogicalSize<f64> =
            (view_frame.size.width as f64, view_frame.size.height as f64).into();
        let scale_factor = self.scale_factor();
        logical.to_physical(scale_factor)
    }

    #[inline]
    pub fn set_inner_size(&self, size: Size) {
        unsafe {
            let scale_factor = self.scale_factor();
            util::set_content_size_async(*self.ns_window, size.to_logical(scale_factor));
        }
    }

    pub fn set_min_inner_size(&self, dimensions: Option<Size>) {
        unsafe {
            let dimensions = dimensions.unwrap_or(Logical(LogicalSize {
                width: 0.0,
                height: 0.0,
            }));
            let scale_factor = self.scale_factor();
            set_min_inner_size(*self.ns_window, dimensions.to_logical(scale_factor));
        }
    }

    pub fn set_max_inner_size(&self, dimensions: Option<Size>) {
        unsafe {
            let dimensions = dimensions.unwrap_or(Logical(LogicalSize {
                width: std::f32::MAX as f64,
                height: std::f32::MAX as f64,
            }));
            let scale_factor = self.scale_factor();
            set_max_inner_size(*self.ns_window, dimensions.to_logical(scale_factor));
        }
    }

    pub fn resize_increments(&self) -> Option<PhysicalSize<u32>> {
        let increments = unsafe { self.ns_window.contentResizeIncrements() };
        let (x, y) = (increments.width, increments.height);
        if x > 1.0 || y > 1.0 {
            Some(LogicalSize::new(x, y).to_physical(self.scale_factor()))
        } else {
            None
        }
    }

    pub fn set_resize_increments(&self, increments: Option<Size>) {
        let size = increments
            .map(|increments| {
                let logical = increments.to_logical::<f64>(self.scale_factor());
                NSSize::new(logical.width.max(1.0), logical.height.max(1.0))
            })
            .unwrap_or_else(|| NSSize::new(1.0, 1.0));
        unsafe {
            self.ns_window.setContentResizeIncrements_(size);
        }
    }

    #[inline]
    pub fn set_resizable(&self, resizable: bool) {
        let fullscreen = {
            let mut shared_state_lock = self.lock_shared_state("set_resizable");
            shared_state_lock.resizable = resizable;
            shared_state_lock.fullscreen.is_some()
        };
        if !fullscreen {
            let mut mask = unsafe { self.ns_window.styleMask() };
            if resizable {
                mask |= NSWindowStyleMask::NSResizableWindowMask;
            } else {
                mask &= !NSWindowStyleMask::NSResizableWindowMask;
            }
            self.set_style_mask_async(mask);
        } // Otherwise, we don't change the mask until we exit fullscreen.
    }

    #[inline]
    pub fn is_resizable(&self) -> bool {
        unsafe { msg_send![*self.ns_window, isResizable] }
    }

    pub fn set_cursor_icon(&self, icon: CursorIcon) {
        let view_state: &ViewState = unsafe {
            let ns_view: &Object = (*self.ns_view).as_ref().expect("failed to deref");
            let state_ptr: *const c_void = *ns_view.ivar("winitState");
            &*(state_ptr as *const ViewState)
        };
        let mut cursor_state = view_state.cursor_state.lock().unwrap();
        cursor_state.cursor = NSCursor::from_icon(icon);
        drop(cursor_state);
        unsafe {
            let _: () = msg_send![
                *self.ns_window,
                invalidateCursorRectsForView: *self.ns_view,
            ];
        }
    }

    #[inline]
    pub fn set_cursor_grab(&self, mode: CursorGrabMode) -> Result<(), ExternalError> {
        let associate_mouse_cursor = match mode {
            CursorGrabMode::Locked => false,
            CursorGrabMode::None => true,
            CursorGrabMode::Confined => {
                return Err(ExternalError::NotSupported(NotSupportedError::new()))
            }
        };

        // TODO: Do this for real https://stackoverflow.com/a/40922095/5435443
        CGDisplay::associate_mouse_and_mouse_cursor_position(associate_mouse_cursor)
            .map_err(|status| ExternalError::Os(os_error!(OsError::CGError(status))))
    }

    #[inline]
    pub fn set_cursor_visible(&self, visible: bool) {
        let view_state: &ViewState = unsafe {
            let ns_view: &Object = (*self.ns_view).as_ref().expect("failed to deref");
            let state_ptr: *const c_void = *ns_view.ivar("winitState");
            &*(state_ptr as *const ViewState)
        };
        let mut cursor_state = view_state.cursor_state.lock().unwrap();
        if visible != cursor_state.visible {
            cursor_state.visible = visible;
            drop(cursor_state);
            unsafe {
                let _: () = msg_send![*self.ns_window,
                    invalidateCursorRectsForView:*self.ns_view
                ];
            }
        }
    }

    #[inline]
    pub fn scale_factor(&self) -> f64 {
        unsafe { NSWindow::backingScaleFactor(*self.ns_window) as _ }
    }

    #[inline]
    pub fn set_cursor_position(&self, cursor_position: Position) -> Result<(), ExternalError> {
        let physical_window_position = self.inner_position().unwrap();
        let scale_factor = self.scale_factor();
        let window_position = physical_window_position.to_logical::<CGFloat>(scale_factor);
        let logical_cursor_position = cursor_position.to_logical::<CGFloat>(scale_factor);
        let point = appkit::CGPoint {
            x: logical_cursor_position.x + window_position.x,
            y: logical_cursor_position.y + window_position.y,
        };
        CGDisplay::warp_mouse_cursor_position(point)
            .map_err(|e| ExternalError::Os(os_error!(OsError::CGError(e))))?;
        CGDisplay::associate_mouse_and_mouse_cursor_position(true)
            .map_err(|e| ExternalError::Os(os_error!(OsError::CGError(e))))?;

        Ok(())
    }

    #[inline]
    pub fn drag_window(&self) -> Result<(), ExternalError> {
        unsafe {
            let event: id = msg_send![NSApp(), currentEvent];
            let _: () = msg_send![*self.ns_window, performWindowDragWithEvent: event];
        }

        Ok(())
    }

    #[inline]
    pub fn set_cursor_hittest(&self, hittest: bool) -> Result<(), ExternalError> {
        unsafe {
            util::set_ignore_mouse_events(*self.ns_window, !hittest);
        }

        Ok(())
    }

    pub(crate) fn is_zoomed(&self) -> bool {
        // because `isZoomed` doesn't work if the window's borderless,
        // we make it resizable temporalily.
        let curr_mask = unsafe { self.ns_window.styleMask() };

        let required =
            NSWindowStyleMask::NSTitledWindowMask | NSWindowStyleMask::NSResizableWindowMask;
        let needs_temp_mask = !curr_mask.contains(required);
        if needs_temp_mask {
            self.set_style_mask_sync(required);
        }

        let is_zoomed = unsafe { msg_send![*self.ns_window, isZoomed] };

        // Roll back temp styles
        if needs_temp_mask {
            self.set_style_mask_async(curr_mask);
        }

        is_zoomed
    }

    fn saved_style(&self, shared_state: &mut SharedState) -> NSWindowStyleMask {
        let base_mask = shared_state
            .saved_style
            .take()
            .unwrap_or_else(|| unsafe { self.ns_window.styleMask() });
        if shared_state.resizable {
            base_mask | NSWindowStyleMask::NSResizableWindowMask
        } else {
            base_mask & !NSWindowStyleMask::NSResizableWindowMask
        }
    }

    /// This is called when the window is exiting fullscreen, whether by the
    /// user clicking on the green fullscreen button or programmatically by
    /// `toggleFullScreen:`
    pub(crate) fn restore_state_from_fullscreen(&self) {
        let mut shared_state_lock = self.lock_shared_state("restore_state_from_fullscreen");

        shared_state_lock.fullscreen = None;

        let maximized = shared_state_lock.maximized;
        let mask = self.saved_style(&mut *shared_state_lock);

        drop(shared_state_lock);

        self.set_style_mask_async(mask);
        self.set_maximized(maximized);
    }

    #[inline]
    pub fn set_minimized(&self, minimized: bool) {
        let is_minimized: bool = unsafe { msg_send![*self.ns_window, isMiniaturized] };
        if is_minimized == minimized {
            return;
        }

        if minimized {
            unsafe {
                NSWindow::miniaturize_(*self.ns_window, *self.ns_window);
            }
        } else {
            unsafe {
                NSWindow::deminiaturize_(*self.ns_window, *self.ns_window);
            }
        }
    }

    #[inline]
    pub fn set_maximized(&self, maximized: bool) {
        let is_zoomed = self.is_zoomed();
        if is_zoomed == maximized {
            return;
        };
        unsafe {
            util::set_maximized_async(
                *self.ns_window,
                is_zoomed,
                maximized,
                Arc::downgrade(&self.shared_state),
            );
        }
    }

    #[inline]
    pub fn fullscreen(&self) -> Option<Fullscreen> {
        let shared_state_lock = self.lock_shared_state("fullscreen");
        shared_state_lock.fullscreen.clone()
    }

    #[inline]
    pub fn is_maximized(&self) -> bool {
        self.is_zoomed()
    }

    #[inline]
    pub fn set_fullscreen(&self, fullscreen: Option<Fullscreen>) {
        let mut shared_state_lock = self.lock_shared_state("set_fullscreen");
        if shared_state_lock.is_simple_fullscreen {
            return;
        }
        if shared_state_lock.in_fullscreen_transition {
            // We can't set fullscreen here.
            // Set fullscreen after transition.
            shared_state_lock.target_fullscreen = Some(fullscreen);
            return;
        }
        let old_fullscreen = shared_state_lock.fullscreen.clone();
        if fullscreen == old_fullscreen {
            return;
        }
        drop(shared_state_lock);

        // If the fullscreen is on a different monitor, we must move the window
        // to that monitor before we toggle fullscreen (as `toggleFullScreen`
        // does not take a screen parameter, but uses the current screen)
        if let Some(ref fullscreen) = fullscreen {
            let new_screen = match fullscreen {
                Fullscreen::Borderless(borderless) => {
                    let RootMonitorHandle { inner: monitor } = borderless
                        .clone()
                        .unwrap_or_else(|| self.current_monitor_inner());
                    monitor
                }
                Fullscreen::Exclusive(RootVideoMode {
                    video_mode: VideoMode { ref monitor, .. },
                }) => monitor.clone(),
            }
            .ns_screen()
            .unwrap();

            unsafe {
                let old_screen = NSWindow::screen(*self.ns_window);
                if old_screen != new_screen {
                    let mut screen_frame: NSRect = msg_send![new_screen, frame];
                    // The coordinate system here has its origin at bottom-left
                    // and Y goes up
                    screen_frame.origin.y += screen_frame.size.height;
                    util::set_frame_top_left_point_async(*self.ns_window, screen_frame.origin);
                }
            }
        }

        if let Some(Fullscreen::Exclusive(ref video_mode)) = fullscreen {
            // Note: `enterFullScreenMode:withOptions:` seems to do the exact
            // same thing as we're doing here (captures the display, sets the
            // video mode, and hides the menu bar and dock), with the exception
            // of that I couldn't figure out how to set the display mode with
            // it. I think `enterFullScreenMode:withOptions:` is still using the
            // older display mode API where display modes were of the type
            // `CFDictionary`, but this has changed, so we can't obtain the
            // correct parameter for this any longer. Apple's code samples for
            // this function seem to just pass in "YES" for the display mode
            // parameter, which is not consistent with the docs saying that it
            // takes a `NSDictionary`..

            let display_id = video_mode.monitor().inner.native_identifier();

            let mut fade_token = ffi::kCGDisplayFadeReservationInvalidToken;

            if matches!(old_fullscreen, Some(Fullscreen::Borderless(_))) {
                unsafe {
                    let app = NSApp();
                    let mut shared_state_lock = self.lock_shared_state("set_fullscreen");
                    shared_state_lock.save_presentation_opts = Some(app.presentationOptions_());
                }
            }

            unsafe {
                // Fade to black (and wait for the fade to complete) to hide the
                // flicker from capturing the display and switching display mode
                if ffi::CGAcquireDisplayFadeReservation(5.0, &mut fade_token)
                    == ffi::kCGErrorSuccess
                {
                    ffi::CGDisplayFade(
                        fade_token,
                        0.3,
                        ffi::kCGDisplayBlendNormal,
                        ffi::kCGDisplayBlendSolidColor,
                        0.0,
                        0.0,
                        0.0,
                        ffi::TRUE,
                    );
                }

                assert_eq!(ffi::CGDisplayCapture(display_id), ffi::kCGErrorSuccess);
            }

            unsafe {
                let result = ffi::CGDisplaySetDisplayMode(
                    display_id,
                    video_mode.video_mode.native_mode.0,
                    std::ptr::null(),
                );
                assert!(result == ffi::kCGErrorSuccess, "failed to set video mode");

                // After the display has been configured, fade back in
                // asynchronously
                if fade_token != ffi::kCGDisplayFadeReservationInvalidToken {
                    ffi::CGDisplayFade(
                        fade_token,
                        0.6,
                        ffi::kCGDisplayBlendSolidColor,
                        ffi::kCGDisplayBlendNormal,
                        0.0,
                        0.0,
                        0.0,
                        ffi::FALSE,
                    );
                    ffi::CGReleaseDisplayFadeReservation(fade_token);
                }
            }
        }

        let mut shared_state_lock = self.lock_shared_state("set_fullscreen");
        shared_state_lock.fullscreen = fullscreen.clone();

        match (&old_fullscreen, &fullscreen) {
            (&None, &Some(_)) => unsafe {
                util::toggle_full_screen_async(
                    *self.ns_window,
                    *self.ns_view,
                    old_fullscreen.is_none(),
                    Arc::downgrade(&self.shared_state),
                );
            },
            (&Some(Fullscreen::Borderless(_)), &None) => unsafe {
                // State is restored by `window_did_exit_fullscreen`
                util::toggle_full_screen_async(
                    *self.ns_window,
                    *self.ns_view,
                    old_fullscreen.is_none(),
                    Arc::downgrade(&self.shared_state),
                );
            },
            (&Some(Fullscreen::Exclusive(RootVideoMode { ref video_mode })), &None) => unsafe {
                util::restore_display_mode_async(video_mode.monitor().inner.native_identifier());
                // Rest of the state is restored by `window_did_exit_fullscreen`
                util::toggle_full_screen_async(
                    *self.ns_window,
                    *self.ns_view,
                    old_fullscreen.is_none(),
                    Arc::downgrade(&self.shared_state),
                );
            },
            (&Some(Fullscreen::Borderless(_)), &Some(Fullscreen::Exclusive(_))) => unsafe {
                // If we're already in fullscreen mode, calling
                // `CGDisplayCapture` will place the shielding window on top of
                // our window, which results in a black display and is not what
                // we want. So, we must place our window on top of the shielding
                // window. Unfortunately, this also makes our window be on top
                // of the menu bar, and this looks broken, so we must make sure
                // that the menu bar is disabled. This is done in the window
                // delegate in `window:willUseFullScreenPresentationOptions:`.
                let app = NSApp();
                shared_state_lock.save_presentation_opts = Some(app.presentationOptions_());

                let presentation_options =
                    NSApplicationPresentationOptions::NSApplicationPresentationFullScreen
                        | NSApplicationPresentationOptions::NSApplicationPresentationHideDock
                        | NSApplicationPresentationOptions::NSApplicationPresentationHideMenuBar;
                app.setPresentationOptions_(presentation_options);

                let _: () = msg_send![*self.ns_window, setLevel: ffi::CGShieldingWindowLevel() + 1];
            },
            (
                &Some(Fullscreen::Exclusive(RootVideoMode { ref video_mode })),
                &Some(Fullscreen::Borderless(_)),
            ) => unsafe {
                let presentation_options =
                    shared_state_lock.save_presentation_opts.unwrap_or_else(|| {
                        NSApplicationPresentationOptions::NSApplicationPresentationFullScreen
                        | NSApplicationPresentationOptions::NSApplicationPresentationAutoHideDock
                        | NSApplicationPresentationOptions::NSApplicationPresentationAutoHideMenuBar
                    });
                NSApp().setPresentationOptions_(presentation_options);

                util::restore_display_mode_async(video_mode.monitor().inner.native_identifier());

                // Restore the normal window level following the Borderless fullscreen
                // `CGShieldingWindowLevel() + 1` hack.
                let _: () = msg_send![*self.ns_window, setLevel: ffi::kCGBaseWindowLevelKey];
            },
            _ => {}
        };
    }

    #[inline]
    pub fn set_decorations(&self, decorations: bool) {
        if decorations != self.decorations.load(Ordering::Acquire) {
            self.decorations.store(decorations, Ordering::Release);

            let (fullscreen, resizable) = {
                let shared_state_lock = self.lock_shared_state("set_decorations");
                (
                    shared_state_lock.fullscreen.is_some(),
                    shared_state_lock.resizable,
                )
            };

            // If we're in fullscreen mode, we wait to apply decoration changes
            // until we're in `window_did_exit_fullscreen`.
            if fullscreen {
                return;
            }

            let new_mask = {
                let mut new_mask = if decorations {
                    NSWindowStyleMask::NSClosableWindowMask
                        | NSWindowStyleMask::NSMiniaturizableWindowMask
                        | NSWindowStyleMask::NSResizableWindowMask
                        | NSWindowStyleMask::NSTitledWindowMask
                } else {
                    NSWindowStyleMask::NSBorderlessWindowMask
                        | NSWindowStyleMask::NSResizableWindowMask
                };
                if !resizable {
                    new_mask &= !NSWindowStyleMask::NSResizableWindowMask;
                }
                new_mask
            };
            self.set_style_mask_async(new_mask);
        }
    }

    #[inline]
    pub fn is_decorated(&self) -> bool {
        self.decorations.load(Ordering::Acquire)
    }

    #[inline]
    pub fn set_always_on_top(&self, always_on_top: bool) {
        let level = if always_on_top {
            ffi::NSWindowLevel::NSFloatingWindowLevel
        } else {
            ffi::NSWindowLevel::NSNormalWindowLevel
        };
        unsafe { util::set_level_async(*self.ns_window, level) };
    }

    #[inline]
    pub fn set_window_icon(&self, _icon: Option<Icon>) {
        // macOS doesn't have window icons. Though, there is
        // `setRepresentedFilename`, but that's semantically distinct and should
        // only be used when the window is in some way representing a specific
        // file/directory. For instance, Terminal.app uses this for the CWD.
        // Anyway, that should eventually be implemented as
        // `WindowBuilderExt::with_represented_file` or something, and doesn't
        // have anything to do with `set_window_icon`.
        // https://developer.apple.com/library/content/documentation/Cocoa/Conceptual/WinPanel/Tasks/SettingWindowTitle.html
    }

    #[inline]
    pub fn set_ime_position(&self, spot: Position) {
        let scale_factor = self.scale_factor();
        let logical_spot = spot.to_logical(scale_factor);
        unsafe { view::set_ime_position(*self.ns_view, logical_spot) };
    }

    #[inline]
    pub fn set_ime_allowed(&self, allowed: bool) {
        unsafe {
            view::set_ime_allowed(*self.ns_view, allowed);
        }
    }

    #[inline]
    pub fn focus_window(&self) {
        let is_minimized: bool = unsafe { msg_send![*self.ns_window, isMiniaturized] };
        let is_visible: bool = unsafe { msg_send![*self.ns_window, isVisible] };

        if !is_minimized && is_visible {
            unsafe {
                NSApp().activateIgnoringOtherApps_(Bool::YES.as_raw());
                util::make_key_and_order_front_async(*self.ns_window);
            }
        }
    }

    #[inline]
    pub fn request_user_attention(&self, request_type: Option<UserAttentionType>) {
        let ns_request_type = request_type.map(|ty| match ty {
            UserAttentionType::Critical => NSRequestUserAttentionType::NSCriticalRequest,
            UserAttentionType::Informational => NSRequestUserAttentionType::NSInformationalRequest,
        });
        unsafe {
            if let Some(ty) = ns_request_type {
                NSApp().requestUserAttention_(ty);
            }
        }
    }

    #[inline]
    // Allow directly accessing the current monitor internally without unwrapping.
    pub(crate) fn current_monitor_inner(&self) -> RootMonitorHandle {
        unsafe {
            let screen: id = msg_send![*self.ns_window, screen];
            let desc = NSScreen::deviceDescription(screen);
            let key = util::ns_string_id_ref("NSScreenNumber");
            let value = NSDictionary::valueForKey_(desc, *key);
            let display_id: NSUInteger = msg_send![value, unsignedIntegerValue];
            RootMonitorHandle {
                inner: MonitorHandle::new(display_id.try_into().unwrap()),
            }
        }
    }

    #[inline]
    pub fn current_monitor(&self) -> Option<RootMonitorHandle> {
        Some(self.current_monitor_inner())
    }

    #[inline]
    pub fn available_monitors(&self) -> VecDeque<MonitorHandle> {
        monitor::available_monitors()
    }

    #[inline]
    pub fn primary_monitor(&self) -> Option<RootMonitorHandle> {
        let monitor = monitor::primary_monitor();
        Some(RootMonitorHandle { inner: monitor })
    }

    #[inline]
    pub fn raw_window_handle(&self) -> RawWindowHandle {
        let mut window_handle = AppKitWindowHandle::empty();
        window_handle.ns_window = *self.ns_window as *mut _;
        window_handle.ns_view = *self.ns_view as *mut _;
        RawWindowHandle::AppKit(window_handle)
    }

    #[inline]
    pub fn raw_display_handle(&self) -> RawDisplayHandle {
        RawDisplayHandle::AppKit(AppKitDisplayHandle::empty())
    }
}

impl WindowExtMacOS for UnownedWindow {
    #[inline]
    fn ns_window(&self) -> *mut c_void {
        *self.ns_window as *mut _
    }

    #[inline]
    fn ns_view(&self) -> *mut c_void {
        *self.ns_view as *mut _
    }

    #[inline]
    fn simple_fullscreen(&self) -> bool {
        let shared_state_lock = self.shared_state.lock().unwrap();
        shared_state_lock.is_simple_fullscreen
    }

    #[inline]
    fn set_simple_fullscreen(&self, fullscreen: bool) -> bool {
        let mut shared_state_lock = self.shared_state.lock().unwrap();

        unsafe {
            let app = NSApp();
            let is_native_fullscreen = shared_state_lock.fullscreen.is_some();
            let is_simple_fullscreen = shared_state_lock.is_simple_fullscreen;

            // Do nothing if native fullscreen is active.
            if is_native_fullscreen
                || (fullscreen && is_simple_fullscreen)
                || (!fullscreen && !is_simple_fullscreen)
            {
                return false;
            }

            if fullscreen {
                // Remember the original window's settings
                // Exclude title bar
                shared_state_lock.standard_frame = Some(NSWindow::contentRectForFrameRect_(
                    *self.ns_window,
                    NSWindow::frame(*self.ns_window),
                ));
                shared_state_lock.saved_style = Some(self.ns_window.styleMask());
                shared_state_lock.save_presentation_opts = Some(app.presentationOptions_());

                // Tell our window's state that we're in fullscreen
                shared_state_lock.is_simple_fullscreen = true;

                // Simulate pre-Lion fullscreen by hiding the dock and menu bar
                let presentation_options =
                    NSApplicationPresentationOptions::NSApplicationPresentationAutoHideDock |
                    NSApplicationPresentationOptions::NSApplicationPresentationAutoHideMenuBar;
                app.setPresentationOptions_(presentation_options);

                // Hide the titlebar
                util::toggle_style_mask(
                    *self.ns_window,
                    *self.ns_view,
                    NSWindowStyleMask::NSTitledWindowMask,
                    false,
                );

                // Set the window frame to the screen frame size
                let screen = self.ns_window.screen();
                let screen_frame = NSScreen::frame(screen);
                NSWindow::setFrame_display_(*self.ns_window, screen_frame, Bool::YES.as_raw());

                // Fullscreen windows can't be resized, minimized, or moved
                util::toggle_style_mask(
                    *self.ns_window,
                    *self.ns_view,
                    NSWindowStyleMask::NSMiniaturizableWindowMask,
                    false,
                );
                util::toggle_style_mask(
                    *self.ns_window,
                    *self.ns_view,
                    NSWindowStyleMask::NSResizableWindowMask,
                    false,
                );
                NSWindow::setMovable_(*self.ns_window, Bool::NO.as_raw());

                true
            } else {
                let new_mask = self.saved_style(&mut *shared_state_lock);
                self.set_style_mask_async(new_mask);
                shared_state_lock.is_simple_fullscreen = false;

                if let Some(presentation_opts) = shared_state_lock.save_presentation_opts {
                    app.setPresentationOptions_(presentation_opts);
                }

                let frame = shared_state_lock.saved_standard_frame();
                NSWindow::setFrame_display_(*self.ns_window, frame, Bool::YES.as_raw());
                NSWindow::setMovable_(*self.ns_window, Bool::YES.as_raw());

                true
            }
        }
    }

    #[inline]
    fn has_shadow(&self) -> bool {
        unsafe { Bool::from_raw(self.ns_window.hasShadow()).as_bool() }
    }

    #[inline]
    fn set_has_shadow(&self, has_shadow: bool) {
        unsafe { self.ns_window.setHasShadow_(Bool::new(has_shadow).as_raw()) }
    }
}

impl Drop for UnownedWindow {
    fn drop(&mut self) {
        trace!("Dropping `UnownedWindow` ({:?})", self as *mut _);
        // Close the window if it has not yet been closed.
        if *self.ns_window != nil {
            unsafe { util::close_async(self.ns_window.clone()) };
        }
    }
}

unsafe fn set_min_inner_size<V: NSWindow + Copy>(window: V, mut min_size: LogicalSize<f64>) {
    let mut current_rect = NSWindow::frame(window);
    let content_rect = NSWindow::contentRectForFrameRect_(window, NSWindow::frame(window));
    // Convert from client area size to window size
    min_size.width += (current_rect.size.width - content_rect.size.width) as f64; // this tends to be 0
    min_size.height += (current_rect.size.height - content_rect.size.height) as f64;
    let min_size = NSSize {
        width: min_size.width as CGFloat,
        height: min_size.height as CGFloat,
    };
    window.setMinSize_(min_size);
    // If necessary, resize the window to match constraint
    if current_rect.size.width < min_size.width {
        current_rect.size.width = min_size.width;
        window.setFrame_display_(current_rect, Bool::NO.as_raw())
    }
    if current_rect.size.height < min_size.height {
        // The origin point of a rectangle is at its bottom left in Cocoa.
        // To ensure the window's top-left point remains the same:
        current_rect.origin.y += current_rect.size.height - min_size.height;
        current_rect.size.height = min_size.height;
        window.setFrame_display_(current_rect, Bool::NO.as_raw())
    }
}

unsafe fn set_max_inner_size<V: NSWindow + Copy>(window: V, mut max_size: LogicalSize<f64>) {
    let mut current_rect = NSWindow::frame(window);
    let content_rect = NSWindow::contentRectForFrameRect_(window, NSWindow::frame(window));
    // Convert from client area size to window size
    max_size.width += (current_rect.size.width - content_rect.size.width) as f64; // this tends to be 0
    max_size.height += (current_rect.size.height - content_rect.size.height) as f64;
    let max_size = NSSize {
        width: max_size.width as CGFloat,
        height: max_size.height as CGFloat,
    };
    window.setMaxSize_(max_size);
    // If necessary, resize the window to match constraint
    if current_rect.size.width > max_size.width {
        current_rect.size.width = max_size.width;
        window.setFrame_display_(current_rect, Bool::NO.as_raw())
    }
    if current_rect.size.height > max_size.height {
        // The origin point of a rectangle is at its bottom left in Cocoa.
        // To ensure the window's top-left point remains the same:
        current_rect.origin.y += current_rect.size.height - max_size.height;
        current_rect.size.height = max_size.height;
        window.setFrame_display_(current_rect, Bool::NO.as_raw())
    }
}
