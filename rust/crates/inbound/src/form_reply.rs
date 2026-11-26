//! Question form answers.
//!
//! The relay does not interpret a question form. The phone text is passed as
//! the cancel reason of the newest pushed form, so the TUI form closes at once
//! and the assistant receives the answer in the words the user typed. The
//! assistant decides whether the answer is enough and asks again if it is not.
//!
//! A form that was settled on the desktop inside the tombstone window answers
//! `already answered`; a transient failure keeps the record so a retry works.

use std::fmt;

use ntfy_relay_core::adapter::{AdapterError, FormApi};
use ntfy_relay_core::forms::{FormRecord, FormStore, Resolve};
use ntfy_relay_core::text::trim_js;

/// What the asker is told when `/answer` carries no text.
pub const EMPTY_ANSWER: &str = "The user dismissed the question without an answer.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormError {
    /// No record and no tombstone.
    NoForm,
    /// The form was settled on the desktop or by an earlier reply.
    AlreadyAnswered { title: String },
    /// The server rejected the cancel for a reason a retry will not fix.
    Server { status: u16, message: String },
    /// Transport failure or a server fault; the record stays.
    Adapter(AdapterError),
}

impl FormError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Server { status, .. } => Some(*status),
            Self::Adapter(error) => error.status(),
            _ => None,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NoForm => "no pending question form".to_string(),
            Self::AlreadyAnswered { title } => {
                format!("that question in {title} was already answered")
            }
            Self::Server { message, .. } => message.clone(),
            Self::Adapter(error) => error.message().to_string(),
        }
    }
}

impl fmt::Display for FormError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for FormError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormOutcome {
    pub title: String,
}

impl FormOutcome {
    pub fn ack_body(&self) -> String {
        format!("answer sent for {}", self.title)
    }
}

/// Cancels the newest pushed form on the reply's topic with the text the phone
/// sent.
pub async fn apply_form_reply(
    forms: &FormStore,
    api: &dyn FormApi,
    topic: &str,
    text: &str,
) -> Result<FormOutcome, FormError> {
    let record = match forms.resolve(None, topic) {
        Resolve::Empty | Resolve::Unknown { .. } => return Err(FormError::NoForm),
        Resolve::AlreadyAnswered(form) => {
            return Err(FormError::AlreadyAnswered { title: form.title })
        }
        Resolve::Found(form) => form,
    };
    let message = if trim_js(text).is_empty() {
        EMPTY_ANSWER
    } else {
        trim_js(text)
    };
    finish(forms, api, record, message).await
}

async fn finish(
    forms: &FormStore,
    api: &dyn FormApi,
    record: FormRecord,
    message: &str,
) -> Result<FormOutcome, FormError> {
    match api
        .cancel_form(&record.session, &record.form, message)
        .await
    {
        Ok(()) => {
            forms.remove(&record.form);
            Ok(FormOutcome {
                title: record.title,
            })
        }
        Err(error) => match error.status() {
            Some(404) | Some(409) => {
                forms.remove(&record.form);
                Err(FormError::AlreadyAnswered {
                    title: record.title,
                })
            }
            Some(status) => Err(FormError::Server {
                status,
                message: error.message().to_string(),
            }),
            None => Err(FormError::Adapter(error)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntfy_relay_core::forms::FormRecord;
    use std::sync::Mutex;

    struct FakeForms {
        cancels: Mutex<Vec<(String, String, String)>>,
        fail: Option<(u16, &'static str)>,
    }

    #[async_trait::async_trait]
    impl FormApi for FakeForms {
        async fn cancel_form(
            &self,
            session_id: &str,
            form_id: &str,
            message: &str,
        ) -> Result<(), AdapterError> {
            if let Some((status, message)) = self.fail {
                return Err(AdapterError::with_status(status, message));
            }
            self.cancels.lock().unwrap().push((
                session_id.to_string(),
                form_id.to_string(),
                message.to_string(),
            ));
            Ok(())
        }
    }

    impl FakeForms {
        fn new() -> Self {
            Self {
                cancels: Mutex::new(Vec::new()),
                fail: None,
            }
        }

        fn failing(status: u16, message: &'static str) -> Self {
            Self {
                cancels: Mutex::new(Vec::new()),
                fail: Some((status, message)),
            }
        }
    }

    fn store(name: &str) -> FormStore {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("temp")
            .join("inbound-form-reply-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        FormStore::open(root.join("forms.json"), 0).0
    }

    fn register(store: &FormStore, form: &str, title: &str, pushed: i64) {
        register_topic(store, form, title, "proj", pushed);
    }

    fn register_topic(store: &FormStore, form: &str, title: &str, topic: &str, pushed: i64) {
        store
            .register(FormRecord {
                code: String::new(),
                form: form.to_string(),
                session: "ses_1".to_string(),
                topic: topic.to_string(),
                title: title.to_string(),
                pushed,
                fields: Vec::new(),
            })
            .unwrap();
    }

    #[tokio::test]
    async fn free_text_cancels_the_newest_form() {
        let store = store("free-text");
        register(&store, "frm_1", "Questions", 1);
        let api = FakeForms::new();
        let outcome = apply_form_reply(&store, &api, "proj", " keep both, any order ")
            .await
            .unwrap();
        assert_eq!(outcome.title, "Questions");
        assert_eq!(outcome.ack_body(), "answer sent for Questions");
        assert_eq!(
            api.cancels.lock().unwrap().as_slice(),
            &[(
                "ses_1".to_string(),
                "frm_1".to_string(),
                "keep both, any order".to_string()
            )]
        );
        assert_eq!(store.count(), 0);
    }

    #[tokio::test]
    async fn the_newest_form_wins() {
        let store = store("newest");
        register(&store, "frm_1", "Old", 10);
        register(&store, "frm_2", "New", 20);
        let api = FakeForms::new();
        let outcome = apply_form_reply(&store, &api, "proj", "either")
            .await
            .unwrap();
        assert_eq!(outcome.title, "New");
        assert_eq!(api.cancels.lock().unwrap()[0].1, "frm_2");
        assert_eq!(store.count(), 1);
    }

    #[tokio::test]
    async fn an_empty_answer_still_dismisses_the_form() {
        let store = store("empty");
        register(&store, "frm_1", "Questions", 1);
        let api = FakeForms::new();
        apply_form_reply(&store, &api, "proj", "   ").await.unwrap();
        assert_eq!(api.cancels.lock().unwrap()[0].2, EMPTY_ANSWER);
        assert_eq!(store.count(), 0);
    }

    #[tokio::test]
    async fn a_next_line_character_is_an_answer_not_whitespace() {
        let store = store("next-line");
        register(&store, "frm_1", "Questions", 1);
        let api = FakeForms::new();
        apply_form_reply(&store, &api, "proj", "\u{0085}")
            .await
            .unwrap();
        assert_eq!(api.cancels.lock().unwrap()[0].2, "\u{0085}");
    }

    #[tokio::test]
    async fn the_newest_form_on_the_reply_topic_wins() {
        let store = store("topic");
        register_topic(&store, "frm_1", "Proj", "proj", 10);
        register_topic(&store, "frm_2", "Other", "other", 20);
        let api = FakeForms::new();
        let outcome = apply_form_reply(&store, &api, "proj", "yes").await.unwrap();
        assert_eq!(outcome.title, "Proj");
        assert_eq!(api.cancels.lock().unwrap()[0].1, "frm_1");
        let outcome = apply_form_reply(&store, &api, "other", "no").await.unwrap();
        assert_eq!(outcome.title, "Other");
        assert_eq!(api.cancels.lock().unwrap()[1].1, "frm_2");
        assert_eq!(store.count(), 0);
    }

    #[tokio::test]
    async fn a_form_on_another_topic_is_not_answered() {
        let store = store("other-topic");
        register_topic(&store, "frm_1", "Other", "other", 10);
        let api = FakeForms::new();
        let error = apply_form_reply(&store, &api, "proj", "yes")
            .await
            .unwrap_err();
        assert_eq!(error, FormError::NoForm);
        assert!(api.cancels.lock().unwrap().is_empty());
        assert_eq!(store.count(), 1);
    }

    #[tokio::test]
    async fn no_form_answers_truthfully() {
        let store = store("none");
        let api = FakeForms::new();
        let error = apply_form_reply(&store, &api, "proj", "yes")
            .await
            .unwrap_err();
        assert_eq!(error, FormError::NoForm);
        assert_eq!(error.message(), "no pending question form");
        assert!(api.cancels.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_tombstone_answers_already_answered() {
        let store = store("tombstone");
        register(&store, "frm_1", "Questions", 1);
        store.remove("frm_1");
        let api = FakeForms::new();
        let error = apply_form_reply(&store, &api, "proj", "yes")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            FormError::AlreadyAnswered {
                title: "Questions".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_settled_form_removes_the_record() {
        let store = store("settled");
        register(&store, "frm_1", "Questions", 1);
        let api = FakeForms::failing(409, "FormAlreadySettledError");
        let error = apply_form_reply(&store, &api, "proj", "yes")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            FormError::AlreadyAnswered {
                title: "Questions".to_string()
            }
        );
        assert_eq!(store.count(), 0);
    }

    #[tokio::test]
    async fn a_server_fault_keeps_the_record() {
        let store = store("fault");
        register(&store, "frm_1", "Questions", 1);
        let api = FakeForms::failing(500, "boom");
        let error = apply_form_reply(&store, &api, "proj", "yes")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            FormError::Server {
                status: 500,
                message: "boom".to_string()
            }
        );
        assert_eq!(store.count(), 1);
    }
}
