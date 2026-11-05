//! The settings window: a small webview form.

use tokenshare_core::settings::Form;

const PAGE: &str = include_str!("settings.html");

/// The page with `form` filled in, plus this machine's hostname to show as
/// the name used when none is set.
pub fn page(form: &Form) -> String {
    let mut init = serde_json::to_value(form).expect("form serializes");
    init["hostname"] = tokenshare_core::Config::default().display_name().into();
    // "</" would end the script block early if a value contained it.
    let json = init.to_string().replace("</", "<\\/");
    PAGE.replace("/*INITIAL*/null", &json)
}

#[derive(serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Message {
    Save {
        form: Box<Form>,
        /// Set after a warning (such as a busy port) to save regardless.
        #[serde(default)]
        force: bool,
    },
    Cancel,
    /// Put `text` on the system clipboard (Cmd+C / Cmd+X in a field).
    Copy {
        text: String,
    },
    /// Ask for the clipboard text (Cmd+V); answered with `insertText(...)`.
    Paste,
    /// The page's content height in logical pixels; the window follows it.
    Resize {
        height: f64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokenshare_core::Config;

    #[test]
    fn page_embeds_the_current_values_safely() {
        let c = Config {
            fc: Some("http://a:1/</script><b>".into()),
            ..Config::default()
        };
        let html = page(&Form::from_config(&c));
        assert!(!html.contains("/*INITIAL*/"));
        assert!(html.contains("http://a:1/<\\/script><b>"));
        assert!(html.contains("\"port\":\"11343\""));
        assert!(html.contains("\"hostname\":"));
    }

    #[test]
    fn a_save_from_an_older_page_without_the_secret_field_parses() {
        let m: Message = serde_json::from_str(
            r#"{"cmd":"save","form":{"fc":"","advertise_url":"","ollama_hosts":"x","port":"1","pool_size":"2","share_with_peers":true,"masquerade":false,"ollama_port":"11434"}}"#,
        )
        .unwrap();
        assert!(matches!(m, Message::Save { form, .. } if form.pool_secret.is_empty()));
    }

    #[test]
    fn resize_message_parses() {
        let m: Message = serde_json::from_str(r#"{"cmd":"resize","height":523}"#).unwrap();
        assert!(matches!(m, Message::Resize { height } if height == 523.0));
    }

    #[test]
    fn messages_parse() {
        let save: Message = serde_json::from_str(
            r#"{"cmd":"save","form":{"fc":"","advertise_url":"","ollama_hosts":"x","port":"1","pool_size":"2","share_with_peers":false,"masquerade":true,"ollama_port":"12434"},"force":true}"#,
        )
        .unwrap();
        assert!(
            matches!(save, Message::Save { form, force } if form.pool_size == "2" && !form.share_with_peers && form.masquerade && form.ollama_port == "12434" && force)
        );
        assert!(matches!(
            serde_json::from_str::<Message>(r#"{"cmd":"cancel"}"#).unwrap(),
            Message::Cancel
        ));
    }
}
