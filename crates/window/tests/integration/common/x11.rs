//! What a user does to a window, through Xlib and `XTest` on the test's X server: find it by
//! title, read its geometry and map state, close it, take its focus away, press a key, click.
#![expect(
    unsafe_code,
    reason = "Xlib is a C API; each call is one documented unsafe block"
)]

use std::ffi::{CStr, CString, c_char, c_int, c_uchar, c_uint, c_ulong};
use std::mem::MaybeUninit;
use std::ptr;

use x11_dl::keysym::{XK_Escape, XK_Return};
use x11_dl::xlib::{self, Display, Xlib};
use x11_dl::xtest::Xf86vmode as XTest;

pub(crate) use x11_dl::xlib::Window;

pub(crate) const ESCAPE: u32 = XK_Escape;
pub(crate) const RETURN: u32 = XK_Return;

pub(crate) struct X {
    xlib: Xlib,
    xtest: XTest,
    display: *mut Display,
}

impl X {
    pub(crate) fn open() -> Self {
        let xlib = Xlib::open().unwrap();
        let xtest = XTest::open().unwrap();
        // SAFETY: a null name opens $DISPLAY; the result is checked before use.
        let display = unsafe { (xlib.XOpenDisplay)(ptr::null()) };
        assert!(!display.is_null(), "no X display; run the tests under Xvfb");
        Self {
            xlib,
            xtest,
            display,
        }
    }

    fn root(&self) -> Window {
        // SAFETY: `display` is open for the lifetime of `self`.
        unsafe { (self.xlib.XDefaultRootWindow)(self.display) }
    }

    /// The top-level window titled `title`. Without a window manager, toplevels are the
    /// root window's children.
    pub(crate) fn find(&self, title: &str) -> Option<Window> {
        let (mut root, mut parent) = (0, 0);
        let mut children: *mut Window = ptr::null_mut();
        let mut count: c_uint = 0;
        // SAFETY: every out-pointer points at a live local.
        let listed = unsafe {
            (self.xlib.XQueryTree)(
                self.display,
                self.root(),
                &raw mut root,
                &raw mut parent,
                &raw mut children,
                &raw mut count,
            )
        };
        if listed == 0 || children.is_null() {
            return None;
        }
        // SAFETY: XQueryTree returned `count` windows at `children`.
        let windows = unsafe { std::slice::from_raw_parts(children, count as usize) }.to_vec();
        // SAFETY: `children` came from Xlib and is freed once.
        unsafe { (self.xlib.XFree)(children.cast()) };
        windows
            .into_iter()
            .find(|&window| self.title(window).as_deref() == Some(title))
    }

    fn title(&self, window: Window) -> Option<String> {
        let mut name: *mut c_char = ptr::null_mut();
        // SAFETY: `name` is a live out-pointer; Xlib allocates the string.
        let found = unsafe { (self.xlib.XFetchName)(self.display, window, &raw mut name) };
        if found == 0 || name.is_null() {
            return None;
        }
        // SAFETY: XFetchName returned a NUL-terminated string.
        let title = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `name` came from Xlib and is freed once.
        unsafe { (self.xlib.XFree)(name.cast()) };
        Some(title)
    }

    fn attributes(&self, window: Window) -> xlib::XWindowAttributes {
        let mut attributes = MaybeUninit::<xlib::XWindowAttributes>::zeroed();
        // SAFETY: `attributes` is a live out-pointer to a zeroed struct of the right type.
        unsafe { (self.xlib.XGetWindowAttributes)(self.display, window, attributes.as_mut_ptr()) };
        // SAFETY: zeroed is a valid XWindowAttributes (integers and null pointers).
        unsafe { attributes.assume_init() }
    }

    pub(crate) fn mapped(&self, window: Window) -> bool {
        self.attributes(window).map_state == xlib::IsViewable
    }

    /// Position on the root window and size.
    pub(crate) fn geometry(&self, window: Window) -> (i32, i32, u32, u32) {
        let attributes = self.attributes(window);
        let (mut x, mut y, mut child) = (0, 0, 0);
        // SAFETY: every out-pointer points at a live local.
        unsafe {
            (self.xlib.XTranslateCoordinates)(
                self.display,
                window,
                self.root(),
                0,
                0,
                &raw mut x,
                &raw mut y,
                &raw mut child,
            )
        };
        let size = |value: i32| u32::try_from(value).unwrap();
        (x, y, size(attributes.width), size(attributes.height))
    }

    /// The size of the first image in the window's `_NET_WM_ICON`, if it has one.
    pub(crate) fn icon_size(&self, window: Window) -> Option<(c_ulong, c_ulong)> {
        let (mut kind, mut format, mut count, mut after): (xlib::Atom, c_int, c_ulong, c_ulong) =
            (0, 0, 0, 0);
        let mut data: *mut c_uchar = ptr::null_mut();
        // SAFETY: every out-pointer points at a live local; Xlib allocates `data`.
        let status = unsafe {
            (self.xlib.XGetWindowProperty)(
                self.display,
                window,
                self.atom("_NET_WM_ICON"),
                0,
                2,
                xlib::False,
                xlib::XA_CARDINAL,
                &raw mut kind,
                &raw mut format,
                &raw mut count,
                &raw mut after,
                &raw mut data,
            )
        };
        if status != 0 || data.is_null() {
            return None;
        }
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "Xlib allocates format-32 data as an array of C longs"
        )]
        // SAFETY: format-32 data arrives as `count` C longs at `data`.
        let values = unsafe {
            std::slice::from_raw_parts(data.cast::<c_ulong>(), usize::try_from(count).unwrap())
        }
        .to_vec();
        // SAFETY: `data` came from Xlib and is freed once.
        unsafe { (self.xlib.XFree)(data.cast()) };
        match values.as_slice() {
            [width, height] if format == 32 => Some((*width, *height)),
            _ => None,
        }
    }

    fn atom(&self, name: &str) -> xlib::Atom {
        let name = CString::new(name).unwrap();
        // SAFETY: `name` is NUL-terminated and outlives the call.
        unsafe { (self.xlib.XInternAtom)(self.display, name.as_ptr(), xlib::False) }
    }

    /// What the window manager sends when the user clicks the close button.
    pub(crate) fn close(&self, window: Window) {
        let mut data = xlib::ClientMessageData::new();
        data.set_long(0, i64::try_from(self.atom("WM_DELETE_WINDOW")).unwrap());
        data.set_long(1, 0);
        let mut event = xlib::XEvent {
            client_message: xlib::XClientMessageEvent {
                type_: xlib::ClientMessage,
                serial: 0,
                send_event: xlib::True,
                display: self.display,
                window,
                message_type: self.atom("WM_PROTOCOLS"),
                format: 32,
                data,
            },
        };
        // SAFETY: `event` is a fully initialised client message for a window of this display.
        unsafe {
            (self.xlib.XSendEvent)(
                self.display,
                window,
                xlib::False,
                xlib::NoEventMask,
                &raw mut event,
            )
        };
        self.sync();
    }

    /// Whether the keyboard focus is on `window` or inside it (GTK focuses a child of its
    /// toplevel).
    pub(crate) fn has_focus(&self, window: Window) -> bool {
        let (mut focus, mut revert) = (0, 0);
        // SAFETY: both out-pointers point at live locals.
        unsafe { (self.xlib.XGetInputFocus)(self.display, &raw mut focus, &raw mut revert) };
        while focus > 1 && focus != self.root() {
            if focus == window {
                return true;
            }
            focus = self.parent(focus);
        }
        false
    }

    fn parent(&self, window: Window) -> Window {
        let (mut root, mut parent) = (0, 0);
        let mut children: *mut Window = ptr::null_mut();
        let mut count: c_uint = 0;
        // SAFETY: every out-pointer points at a live local.
        let listed = unsafe {
            (self.xlib.XQueryTree)(
                self.display,
                window,
                &raw mut root,
                &raw mut parent,
                &raw mut children,
                &raw mut count,
            )
        };
        if !children.is_null() {
            // SAFETY: `children` came from Xlib and is freed once.
            unsafe { (self.xlib.XFree)(children.cast()) };
        }
        if listed == 0 { 0 } else { parent }
    }

    /// Gives the focus to the root window, as clicking elsewhere on the desktop does.
    pub(crate) fn unfocus(&self) {
        // SAFETY: the root window exists for as long as the display is open.
        unsafe {
            (self.xlib.XSetInputFocus)(
                self.display,
                self.root(),
                xlib::RevertToParent,
                xlib::CurrentTime,
            )
        };
        self.sync();
    }

    /// Presses and releases a key, as the keyboard would, into the focused window.
    pub(crate) fn key(&self, keysym: u32) {
        // SAFETY: a keysym lookup on an open display.
        let code = unsafe { (self.xlib.XKeysymToKeycode)(self.display, c_ulong::from(keysym)) };
        for pressed in [xlib::True, xlib::False] {
            // SAFETY: a synthetic key event for a keycode of this display.
            unsafe { (self.xtest.XTestFakeKeyEvent)(self.display, c_uint::from(code), pressed, 0) };
        }
        self.sync();
    }

    /// Moves the pointer to `(x, y)` on the root window and clicks the left button there.
    pub(crate) fn click(&self, x: i32, y: i32) {
        // SAFETY: a synthetic pointer motion on this display's default screen.
        unsafe { (self.xtest.XTestFakeMotionEvent)(self.display, -1, x, y, 0) };
        for pressed in [xlib::True, xlib::False] {
            // SAFETY: a synthetic press or release of button 1.
            unsafe { (self.xtest.XTestFakeButtonEvent)(self.display, 1, pressed, 0) };
        }
        self.sync();
    }

    fn sync(&self) {
        // SAFETY: flushes the open display's request queue.
        unsafe { (self.xlib.XSync)(self.display, xlib::False) };
    }
}

impl Drop for X {
    fn drop(&mut self) {
        // SAFETY: closes the display this value opened, once.
        unsafe { (self.xlib.XCloseDisplay)(self.display) };
    }
}
