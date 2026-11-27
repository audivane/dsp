//! When the helper windows (the tray's volume popup, our own on-screen
//! display) are freed, and what happens to a show request while a window is
//! being destroyed. Each webview is a web process of its own (see the `app`
//! module docs), so a hidden one is destroyed a while after it was last
//! hidden and made again when needed; the delay keeps a quick reopen instant.

use std::time::Duration;

/// How long a hidden helper window is kept before it is destroyed.
pub const IDLE_FREE: Duration = Duration::from_secs(30);

/// One helper window's hide/show history: a hide hands out a ticket, and
/// the window may be destroyed when the ticket comes due only if nothing
/// happened since (it was not shown, hidden again or freed meanwhile).
#[derive(Debug, Default)]
pub struct IdleReaper {
    generation: u64,
    hidden: bool,
}

impl IdleReaper {
    /// The window was shown (or made): any pending ticket is void.
    pub fn shown(&mut self) {
        self.generation += 1;
        self.hidden = false;
    }

    /// The window was hidden: the ticket to check after [`IDLE_FREE`].
    pub fn hidden(&mut self) -> u64 {
        self.generation += 1;
        self.hidden = true;
        self.generation
    }

    /// The ticket from the last hide is due and the window is still hidden.
    pub fn due(&self, ticket: u64) -> bool {
        self.hidden && self.generation == ticket
    }

    /// The window is gone (destroyed by us or by anything else).
    pub fn freed(&mut self) {
        self.generation += 1;
        self.hidden = false;
    }
}

/// A window on its way out. Tauri queues `destroy()`, so between the
/// decision and the `Destroyed` event the old window can still be found;
/// a show request in that gap (a tray click, a second launch) must not
/// present it. Requests made while closing are kept (the newest wins) and
/// handed back once the window is gone, to be made afresh.
#[derive(Debug)]
pub struct Closing<T> {
    closing: bool,
    queued: bool,
    pending: Option<T>,
}

impl<T> Default for Closing<T> {
    fn default() -> Self {
        Self {
            closing: false,
            queued: false,
            pending: None,
        }
    }
}

impl<T> Closing<T> {
    /// Start closing. False when it already is (a second close request).
    pub fn begin(&mut self) -> bool {
        !std::mem::replace(&mut self.closing, true)
    }

    pub fn is_closing(&self) -> bool {
        self.closing
    }

    /// The destruction itself is about to be queued: true only the first
    /// time per close (the page's answer and its time-out can both ask).
    pub fn queue(&mut self) -> bool {
        self.closing && !std::mem::replace(&mut self.queued, true)
    }

    /// A show request: handed back to run now, or kept while closing.
    pub fn request(&mut self, req: T) -> Option<T> {
        if self.closing {
            self.pending = Some(req);
            None
        } else {
            Some(req)
        }
    }

    /// The window is gone (or its destruction failed): closing ends, and
    /// the request kept meanwhile, if any, is to be replayed.
    pub fn done(&mut self) -> Option<T> {
        self.closing = false;
        self.queued = false;
        self.pending.take()
    }
}
