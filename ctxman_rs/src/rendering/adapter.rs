use super::anthropic::AnthropicMessagesAdapter;
use super::model::{RenderModel, RenderResult};
use super::openai::OpenAiChatAdapter;

/// Provider-Adapter (Spec §4.6): erzeugt aus dem neutralen [`RenderModel`] das Wire-Format
/// eines konkreten LLM-Providers. ctxman ist provider-agnostisch; das Domänenmodell kennt
/// keinen Provider, erst der Adapter mappt Rollen/Kinds, platziert System-Prompt und
/// Tool-Defs und empfiehlt Cache-Breakpoints. Adapter sind zustandslos (Spec §11).
pub trait ProviderAdapter: Send + Sync {
    /// Offener Provider-Bezeichner, gegen den `render` auflöst (Spec §4.3).
    fn provider(&self) -> &'static str;

    fn render(&self, model: &RenderModel) -> RenderResult;
}

/// Löst den offenen `provider`-String auf die eingebauten, zustandslosen Adapter auf
/// (Ersatz der DI-`ProviderAdapterRegistry`; unbekannter Provider ⇒ `None`, der Aufrufer
/// erzeugt daraus `CtxmanError::UnknownProvider`).
pub fn adapter_for(provider: &str) -> Option<&'static dyn ProviderAdapter> {
    static ANTHROPIC: AnthropicMessagesAdapter = AnthropicMessagesAdapter;
    static OPENAI: OpenAiChatAdapter = OpenAiChatAdapter;

    match provider {
        "anthropic" => Some(&ANTHROPIC),
        "openai" => Some(&OPENAI),
        _ => None,
    }
}

/// Alphabetisch sortierte Liste der eingebauten Provider (für Fehlermeldungen, Spec §4.3).
pub fn registered_providers() -> &'static [&'static str] {
    &["anthropic", "openai"]
}
