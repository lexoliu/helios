//! The answer a cancellable host call waits for from an owner task that
//! makes something on its behalf.
//!
//! A guest that drops an import's future cancels the host call at
//! whatever `.await` it is parked on. When that wait is for an owner
//! task's reply to a request that creates something — a display
//! engine's frame buffer resource, a 3D blob, an aperture placement —
//! the thing exists on the device whether or not anybody reads the
//! reply, and nothing in the store records it. [`PendingReply`] is the
//! record until the call keeps what was made in a handle of its own.
//!
//! # How nothing is lost
//!
//! The owner task undoes what it made when its reply cannot be
//! delivered, because the receiving end is gone or closed. Dropping a
//! pending reply closes the receiver first and then looks for an answer
//! that was delivered before the close: closing makes every later send
//! fail, so an answer is either in the channel, and this undoes it, or
//! never delivered, and the owner task does. Neither path needs the
//! store, so a future the guest dropped — or a store dropped with it —
//! takes them.
//!
//! # Concurrency contract
//!
//! Owned by the host call's future, on the store's task. The receiver
//! is a `futures` oneshot, whose close and receive are the only shared
//! state and are atomic; the undo runs on whatever processor drops the
//! future and never waits.

use futures::channel::oneshot;

/// An owner task's answer to one request that makes something, held by
/// the host call waiting for it.
///
/// `undo` is how what was made goes back without the store: it hands
/// the owner task a request that destroys the thing, and never waits.
pub(crate) struct PendingReply<T, E, Undo>
where
    T: Copy,
    Undo: FnOnce(T),
{
    answer: oneshot::Receiver<Result<T, E>>,
    /// What the answer made, once it has arrived.
    made: Option<T>,
    /// Absent once nothing remains to undo: the owner task made
    /// nothing, or the call kept what it made.
    undo: Option<Undo>,
}

impl<T, E, Undo> PendingReply<T, E, Undo>
where
    T: Copy,
    Undo: FnOnce(T),
{
    /// Hold `answer`, the receiving end of a request about to be
    /// queued, with `undo` for whatever it turns out to have made.
    pub(crate) const fn new(answer: oneshot::Receiver<Result<T, E>>, undo: Undo) -> Self {
        Self {
            answer,
            made: None,
            undo: Some(undo),
        }
    }

    /// Wait for the owner task's answer.
    ///
    /// What an agreeing answer made stays this reply's to undo until
    /// [`Self::keep`]. A refusal made nothing, and neither did a request
    /// the owner task dropped without answering — one that outlived its
    /// claim — which is answered `closed`.
    pub(crate) async fn reply(&mut self, closed: E) -> Result<T, E> {
        let answer = (&mut self.answer).await;
        match answer {
            Ok(Ok(made)) => {
                self.made = Some(made);
                Ok(made)
            }
            Ok(Err(error)) => {
                self.undo = None;
                Err(error)
            }
            Err(oneshot::Canceled) => {
                self.undo = None;
                Err(closed)
            }
        }
    }

    /// The call has recorded what the answer made in a handle of its
    /// own, whose drop lets it go from here on.
    pub(crate) fn keep(mut self) {
        self.undo = None;
    }
}

impl<T, E, Undo> Drop for PendingReply<T, E, Undo>
where
    T: Copy,
    Undo: FnOnce(T),
{
    fn drop(&mut self) {
        let Some(undo) = self.undo.take() else {
            return;
        };
        let made = match self.made {
            Some(made) => Some(made),
            None => {
                // Every send after this fails, and the owner task undoes
                // what it made itself; an answer that got here first is
                // this drop's to undo.
                self.answer.close();
                match self.answer.try_recv() {
                    Ok(Some(Ok(made))) => Some(made),
                    Ok(Some(Err(_)) | None) | Err(oneshot::Canceled) => None,
                }
            }
        };
        if let Some(made) = made {
            undo(made);
        }
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use futures::channel::oneshot;
    use futures_lite::future::block_on;

    use super::PendingReply;

    /// The owner task's end of one request.
    type Reply = oneshot::Sender<Result<u32, ()>>;

    /// A pending reply whose undo records what it was told to undo.
    fn armed<'a>(
        undone: &'a Cell<Option<u32>>,
    ) -> (Reply, PendingReply<u32, (), impl FnOnce(u32) + 'a>) {
        let (reply, answer) = oneshot::channel();
        (
            reply,
            PendingReply::new(answer, |made| undone.set(Some(made))),
        )
    }

    /// Dropped before the owner task answered: every later answer is
    /// refused, which is how the owner task learns to undo it itself, and
    /// nothing is undone twice.
    #[test]
    fn a_call_dropped_before_the_answer_leaves_the_undo_to_the_owner() {
        let undone = Cell::new(None);
        let (reply, pending) = armed(&undone);
        drop(pending);
        assert!(reply.is_canceled());
        assert_eq!(reply.send(Ok(7)), Err(Ok(7)));
        assert_eq!(undone.get(), None);
    }

    /// Dropped with an answer delivered but not yet read: the owner task
    /// believes it was handed over, so the drop undoes it.
    #[test]
    fn an_answer_delivered_before_the_drop_is_undone_by_the_drop() {
        let undone = Cell::new(None);
        let (reply, pending) = armed(&undone);
        assert_eq!(reply.send(Ok(7)), Ok(()));
        drop(pending);
        assert_eq!(undone.get(), Some(7));
    }

    /// Dropped after reading the answer but before the call kept it in a
    /// handle — the last step refused, or a wait after the answer was
    /// cancelled — undoes it.
    #[test]
    fn an_answer_read_but_not_kept_is_undone() {
        let undone = Cell::new(None);
        let (reply, mut pending) = armed(&undone);
        assert_eq!(reply.send(Ok(7)), Ok(()));
        assert_eq!(block_on(pending.reply(())), Ok(7));
        drop(pending);
        assert_eq!(undone.get(), Some(7));
    }

    /// Kept, or refused, nothing is undone: the handle owns it, or
    /// nothing was made.
    #[test]
    fn a_kept_answer_or_a_refusal_undoes_nothing() {
        let undone = Cell::new(None);
        let (reply, mut pending) = armed(&undone);
        assert_eq!(reply.send(Ok(7)), Ok(()));
        assert_eq!(block_on(pending.reply(())), Ok(7));
        pending.keep();
        assert_eq!(undone.get(), None);

        let (reply, mut pending) = armed(&undone);
        assert_eq!(reply.send(Err(())), Ok(()));
        assert_eq!(block_on(pending.reply(())), Err(()));
        drop(pending);
        assert_eq!(undone.get(), None);

        let (reply, mut pending) = armed(&undone);
        drop(reply);
        assert_eq!(block_on(pending.reply(())), Err(()));
        drop(pending);
        assert_eq!(undone.get(), None);
    }
}
