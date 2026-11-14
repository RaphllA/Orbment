use crate::i18n::tr;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;

type Handler = Box<dyn Fn(Value) -> Result<Value>>;
#[derive(Default)]
pub struct ActionBus {
    handlers: BTreeMap<String, Handler>,
}
impl ActionBus {
    pub fn register(
        &mut self,
        name: &str,
        handler: impl Fn(Value) -> Result<Value> + 'static,
    ) -> Result<()> {
        let Some((module, action)) = name.split_once('.') else {
            bail!("{}", tr("action.invalid_name"));
        };
        if module.is_empty() || action.is_empty() || self.handlers.contains_key(name) {
            bail!("{}: {name}", tr("action.duplicate"));
        }
        self.handlers.insert(name.into(), Box::new(handler));
        Ok(())
    }
    pub fn call(&self, name: &str, parameters: Value) -> Result<Value> {
        self.handlers
            .get(name)
            .with_context(|| format!("{}: {name}", tr("action.unknown")))?(parameters)
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.handlers.keys().map(String::as_str)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_parameters_and_errors() -> Result<()> {
        let mut bus = ActionBus::default();
        bus.register("audio.switch_playback", |p| Ok(p["device"].clone()))?;
        assert_eq!(
            bus.call(
                "audio.switch_playback",
                serde_json::json!({"device":"speaker"})
            )?,
            "speaker"
        );
        assert!(bus.call("audio.missing", Value::Null).is_err());
        assert!(
            bus.register("audio.switch_playback", |_| Ok(Value::Null))
                .is_err()
        );
        bus.register("app.fail", |_| bail!("failure"))?;
        assert_eq!(
            bus.call("app.fail", Value::Null).unwrap_err().to_string(),
            "failure"
        );
        Ok(())
    }
}
