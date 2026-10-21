//! Awaited, owner-bound UI replies; the legacy polling ABI remains available.
use super::UiDialogMailbox;
use crate::native::NativeOwner;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct PendingDialog {
    mailbox: UiDialogMailbox,
    id: String,
}
impl Drop for PendingDialog {
    fn drop(&mut self) {
        let mut state = self.mailbox.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active.remove(&self.id);
        state.pending.retain(|pending| !pending.is_dialog(&self.id));
        self.mailbox.changed.notify_waiters();
    }
}

impl UiDialogMailbox {
    pub(crate) fn cancel_owner(&self, owner: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.retired_owners.insert(owner);
        state.pending.retain(|pending| !matches!(pending, super::PendingUi::Notification{owner:known,..} if *known == owner));
        for entry in state.active.values_mut() {
            if entry.owner.as_ref().is_some_and(|known| known.id == owner) {
                entry.cancelled = true;
                entry.answer = None;
            }
        }
        self.changed.notify_waiters();
    }

    pub(crate) async fn request(
        &self,
        owner: NativeOwner,
        args: Value,
        signal: CancellationToken,
    ) -> Result<Value, String> {
        if args["action"] != "show" {
            return Err("unsupported native dialog action".into());
        }
        let ui = args
            .get("ui")
            .filter(|ui| ui.is_object())
            .ok_or("missing dialog specification")?;
        let dialog = rpi_plugin_sdk::dialogs::native::Dialog::parse(ui)?;
        if signal.is_cancelled() {
            return Err("native dialog cancelled".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let opened = self.handle_owned(
            json!({"op":"open","requestId":id,"toolCallId":args.get("toolCallId"),"ui":{"nativeDialog":dialog}}),
            Some(owner.clone()),
        )?;
        if opened["status"] == "cancelled" {
            return Ok(Value::Null);
        }
        let _pending = PendingDialog {
            mailbox: self.clone(),
            id: id.clone(),
        };
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let response = self.poll_owned(&id, Some(&owner))?;
            match response["status"].as_str() {
                Some("answered") => return Ok(response["answer"].clone()),
                Some("cancelled") => return Ok(Value::Null),
                _ => {}
            }
            tokio::select! {
                biased;
                _ = signal.cancelled() => return Err("native dialog cancelled".into()),
                _ = changed => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner(id: u64) -> NativeOwner {
        NativeOwner {
            generation: "session".into(),
            id,
            plugin: format!("plugin-{id}"),
            child: false,
        }
    }
    fn prompt() -> Value {
        json!({"action":"show","ui":{"kind":"input","title":"Login code"}})
    }
    async fn pending(mailbox: &UiDialogMailbox) -> super::super::UiDialogRequest {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(request) = mailbox.take_pending() {
                    return request;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn private_reply_cannot_be_consumed_or_replaced_through_legacy_actions() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let task = tokio::spawn({
            let mailbox = mailbox.clone();
            async move {
                mailbox
                    .request(owner(1), prompt(), CancellationToken::new())
                    .await
            }
        });
        let request = pending(&mailbox).await;
        assert!(mailbox.poll(&request.request_id).is_err());
        for op in ["open", "poll", "response", "cancel"] {
            assert!(mailbox
                .handle(json!({"op":op,"requestId":request.request_id,"answer":"stolen"}))
                .is_err());
        }
        assert!(mailbox
            .poll_owned(&request.request_id, Some(&owner(2)))
            .is_err());
        assert!(mailbox
            .respond(
                &request.request_id,
                json!({"type":"confirm","confirmed":true})
            )
            .is_err());
        assert!(mailbox.is_pending(&request.request_id));
        mailbox
            .respond(
                &request.request_id,
                json!({"type":"input","value":"private-login-code"}),
            )
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap()["value"], "private-login-code");
        assert!(!mailbox.is_pending(&request.request_id));
        assert!(mailbox.state.lock().unwrap().active.is_empty());
    }
    #[tokio::test]
    async fn cancellation_drop_detach_and_owner_shutdown_release_awaited_dialogs() {
        for mode in ["cancel", "drop", "detach", "owner"] {
            let mailbox = UiDialogMailbox::new();
            mailbox.attach();
            let signal = CancellationToken::new();
            let task = tokio::spawn({
                let mailbox = mailbox.clone();
                let signal = signal.clone();
                async move { mailbox.request(owner(1), prompt(), signal).await }
            });
            let request = pending(&mailbox).await;
            match mode {
                "cancel" => signal.cancel(),
                "drop" => task.abort(),
                "detach" => mailbox.detach(),
                _ => mailbox.cancel_owner(1),
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap();
            match mode {
                "cancel" => assert!(result.unwrap().is_err()),
                "drop" => assert!(result.unwrap_err().is_cancelled()),
                _ => assert_eq!(result.unwrap().unwrap(), Value::Null),
            }
            assert!(!mailbox.is_pending(&request.request_id));
            assert!(mailbox.state.lock().unwrap().active.is_empty());
            if mode == "owner" {
                assert!(mailbox
                    .request(owner(1), prompt(), CancellationToken::new())
                    .await
                    .is_err());
            }
        }
    }
    #[tokio::test]
    async fn owner_shutdown_leaves_another_owners_question_active() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let first = tokio::spawn({
            let mailbox = mailbox.clone();
            async move {
                mailbox
                    .request(owner(1), prompt(), CancellationToken::new())
                    .await
            }
        });
        let _ = pending(&mailbox).await;
        let second = tokio::spawn({
            let mailbox = mailbox.clone();
            async move {
                mailbox
                    .request(owner(2), prompt(), CancellationToken::new())
                    .await
            }
        });
        let request = pending(&mailbox).await;
        mailbox.cancel_owner(1);
        assert_eq!(first.await.unwrap().unwrap(), Value::Null);
        assert!(mailbox.is_pending(&request.request_id));
        mailbox
            .respond(
                &request.request_id,
                json!({"type":"input","value":"second"}),
            )
            .unwrap();
        assert_eq!(second.await.unwrap().unwrap()["value"], "second");
    }

    #[test]
    fn native_dialog_capacity_is_per_owner_and_releases_after_cancelled_poll() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let open = |id: String, owner| {
            mailbox.handle_owned(json!({"op":"open","requestId":id,"ui":{}}), Some(owner))
        };
        for id in 0..64 {
            open(format!("first-{id}"), owner(1)).unwrap();
        }
        assert!(open("exhausted".into(), owner(1)).is_err());
        open("second".into(), owner(2)).unwrap();
        mailbox.cancel("first-0").unwrap();
        mailbox.poll_owned("first-0", Some(&owner(1))).unwrap();
        open("available".into(), owner(1)).unwrap();
    }
}

impl UiDialogMailbox {
    /// Notices and dialogs share their publication order; notices have no reply or transcript entry.
    pub fn take_notification(&self) -> Option<super::UiNotification> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(
            state.pending.front(),
            Some(super::PendingUi::Notification { .. })
        ) {
            return None;
        }
        match state.pending.pop_front().unwrap() {
            super::PendingUi::Notification { value, .. } => Some(value),
            _ => unreachable!(),
        }
    }
    pub(crate) fn notify(&self, owner: NativeOwner, args: &Value) -> Result<Value, String> {
        if args["action"] != "notify" {
            return Err("unsupported native UI action".into());
        }
        let text = args["text"].as_str().ok_or("missing notification text")?;
        let kind = args
            .get("kind")
            .map_or(Some("info"), Value::as_str)
            .filter(|kind| matches!(*kind, "info" | "warning" | "error"))
            .ok_or("invalid notification kind")?;
        if text.len() > 1024 * 1024 {
            return Err("notification exceeds size limit".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.retired_owners.contains(&owner.id) {
            return Err("notification owner expired".into());
        }
        if !state.attached {
            return Ok(json!({"ok":true}));
        }
        let mut owned = 0;
        let mut bytes = 0;
        let mut total = 0;
        for item in &state.pending {
            if let super::PendingUi::Notification {
                owner: known,
                value,
            } = item
            {
                total += 1;
                bytes += value.text.len();
                owned += usize::from(*known == owner.id);
            }
        }
        if owned >= 128 || total >= 512 || bytes + text.len() > 4 * 1024 * 1024 {
            return Err("notification capacity exhausted".into());
        }
        state.pending.push_back(super::PendingUi::Notification {
            owner: owner.id,
            value: super::UiNotification {
                text: text.into(),
                kind: kind.into(),
            },
        });
        self.changed.notify_waiters();
        Ok(json!({"ok":true}))
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    fn owner(id: u64) -> NativeOwner {
        NativeOwner {
            generation: "session".into(),
            id,
            plugin: format!("owner-{id}"),
            child: false,
        }
    }
    fn notice(text: &str) -> Value {
        json!({"action":"notify","text":text,"kind":"info"})
    }
    #[test]
    fn notices_keep_dialog_order_survive_dialog_cancellation_and_retire_with_their_owner() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.notify(owner(1), &notice("before")).unwrap();
        mailbox
            .handle_owned(
                json!({"op":"open","requestId":"dialog","ui":{"kind":"input","title":"fixture"}}),
                Some(owner(1)),
            )
            .unwrap();
        mailbox.notify(owner(1), &notice("after")).unwrap();
        assert!(mailbox.take_pending().is_none());
        assert_eq!(mailbox.take_notification().unwrap().text, "before");
        assert!(mailbox.take_notification().is_none());
        assert_eq!(mailbox.take_pending().unwrap().request_id, "dialog");
        mailbox.cancel_all();
        assert_eq!(mailbox.take_notification().unwrap().text, "after");
        mailbox.notify(owner(1), &notice("retired")).unwrap();
        mailbox.notify(owner(2), &notice("survivor")).unwrap();
        mailbox.cancel_owner(1);
        assert_eq!(mailbox.take_notification().unwrap().text, "survivor");
        assert!(mailbox.notify(owner(1), &notice("late")).is_err());
        mailbox.detach();
        assert!(mailbox.notify(owner(2), &notice("headless")).is_ok());
        assert!(mailbox.take_notification().is_none());
    }
    #[test]
    fn notification_validation_and_count_byte_limits_are_explicit() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        for args in [
            json!({"action":"other","text":"message"}),
            json!({"action":"notify","text":null}),
            json!({"action":"notify","text":"text","kind":"unknown"}),
            notice(&"x".repeat(1024 * 1024 + 1)),
        ] {
            assert!(mailbox.notify(owner(0), &args).is_err());
        }
        for id in 0..4 {
            for _ in 0..128 {
                mailbox.notify(owner(id), &notice("bounded")).unwrap();
            }
            assert!(mailbox.notify(owner(id), &notice("overflow")).is_err());
        }
        assert!(mailbox.notify(owner(4), &notice("total overflow")).is_err());
        for _ in 0..512 {
            mailbox.take_notification().unwrap();
        }
        let big = "é".repeat(512 * 1024);
        for id in 0..4 {
            mailbox.notify(owner(id), &notice(&big)).unwrap();
        }
        assert!(mailbox.notify(owner(4), &notice("byte overflow")).is_err());
        mailbox.take_notification().unwrap();
        mailbox.notify(owner(4), &notice("room again")).unwrap();
    }
}
