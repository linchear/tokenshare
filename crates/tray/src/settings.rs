//! The settings window: a small webview form.

use tokenshare_core::settings::Form;

const PAGE: &str = include_str!("settings.html");

/// The page with `form` filled in.
pub fn page(form: &Form) -> String {
    // "</" would end the script block early if a value contained it.
    let json = serde_json::to_string(form)
        .expect("form serializes")
        .replace("</", "<\\/");
    PAGE.replace("/*INITIAL*/null", &json)
}

#[derive(serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Message {
    Save {
        form: Form,
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
    }

    #[test]
    fn resize_message_parses() {
        let m: Message = serde_json::from_str(r#"{"cmd":"resize","height":523}"#).unwrap();
        assert!(matches!(m, Message::Resize { height } if height == 523.0));
    }

    #[test]
    fn messages_parse() {
        let save: Message = serde_json::from_str(
            r#"{"cmd":"save","form":{"fc":"","advertise_url":"","ollama_hosts":"x","port":"1","pool_size":"2","share_with_peers":false}}"#,
        )
        .unwrap();
        assert!(
            matches!(save, Message::Save { form } if form.pool_size == "2" && !form.share_with_peers)
        );
        assert!(matches!(
            serde_json::from_str::<Message>(r#"{"cmd":"cancel"}"#).unwrap(),
            Message::Cancel
        ));
    }
}
