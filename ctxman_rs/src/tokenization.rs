//! Pluggable Token-Zählung (Spec §8). Exakte Zählung ist nicht kritisch — Watermarks sind
//! Verhältniswerte — daher genügt eine konservative Approximation; provider-genaue Tokenizer
//! sind eigene Implementierungen (Feature `tiktoken`).

/// Bildet einen Content-String auf die geschätzte Anzahl Tokens ab (Port von `ITokenCounter`).
pub trait TokenCounter: Send + Sync {
    /// Schätzt die Token-Anzahl des übergebenen Inhalts.
    fn count(&self, content: &str) -> u32;
}

/// Konservative Default-Heuristik (Spec §8): ~4 Zeichen pro Token, aufgerundet. Liefert für
/// nicht-leeren Inhalt stets ≥ 1, für leeren Inhalt 0. Zählt UTF-16-Code-Units wie
/// `string.Length` im C#-Original (`HeuristicTokenCounter.cs`) — für ASCII identisch mit
/// Byte-/Zeichen-Zählung.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicTokenCounter;

impl TokenCounter for HeuristicTokenCounter {
    fn count(&self, content: &str) -> u32 {
        let units = content.encode_utf16().count();
        // ceil(units / 4); für units > 0 immer ≥ 1.
        units.div_ceil(4) as u32
    }
}

/// Provider-genauer Tokenizer über tiktoken (Feature `tiktoken`). Default ist die
/// o200k_base-Kodierung; cl100k_base ist wählbar.
#[cfg(feature = "tiktoken")]
pub struct TiktokenCounter {
    bpe: tiktoken_rs::CoreBPE,
}

#[cfg(feature = "tiktoken")]
impl TiktokenCounter {
    /// o200k_base (GPT-4o-Familie) — sinnvoller Default für moderne Modelle.
    pub fn o200k() -> Self {
        TiktokenCounter {
            bpe: tiktoken_rs::o200k_base().expect("eingebettete o200k_base-Daten sind gültig"),
        }
    }

    /// cl100k_base (GPT-3.5/4-Familie).
    pub fn cl100k() -> Self {
        TiktokenCounter {
            bpe: tiktoken_rs::cl100k_base().expect("eingebettete cl100k_base-Daten sind gültig"),
        }
    }
}

#[cfg(feature = "tiktoken")]
impl TokenCounter for TiktokenCounter {
    fn count(&self, content: &str) -> u32 {
        self.bpe.encode_ordinary(content).len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leerer_inhalt_ist_null_tokens() {
        assert_eq!(HeuristicTokenCounter.count(""), 0);
    }

    #[test]
    fn nicht_leer_ist_mindestens_ein_token() {
        assert_eq!(HeuristicTokenCounter.count("a"), 1);
        assert_eq!(HeuristicTokenCounter.count("abcd"), 1);
        assert_eq!(HeuristicTokenCounter.count("abcde"), 2);
    }

    #[test]
    fn zaehlt_utf16_code_units_wie_csharp() {
        // "𝄞" (U+1D11E) ist in UTF-16 ein Surrogate-Paar (2 Units) — C# string.Length = 2.
        assert_eq!(HeuristicTokenCounter.count("𝄞"), 1); // ceil(2/4) = 1
        assert_eq!(HeuristicTokenCounter.count("𝄞𝄞𝄞"), 2); // ceil(6/4) = 2
    }
}
