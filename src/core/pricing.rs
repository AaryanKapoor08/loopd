//! Pricing — the cost fallback when an agent reports tokens but not dollars.
//!
//! **Cost precedence (ARCHITECTURE.md §4):** prefer the agent-reported number.
//! Claude Code's stream-json `result` line carries `total_cost_usd` directly, so
//! for Mode-A Claude runs this module is never consulted. Codex (`exec --json`),
//! the Mode-B transcript (`message.usage`), and the SDK report **tokens only** —
//! for those, [`cost_of`] turns token counts into a dollar estimate so the cost
//! cap means the same thing on every surface. The resolved value is stored on
//! `Run.cost_usd`.
//!
//! Prices are USD per **million** tokens and must be kept current; re-verify on
//! model releases. The Claude figures are from the Anthropic pricing reference
//! (cached 2026-06). Non-Anthropic entries (for the Codex adapter) are
//! best-effort and flagged below — verify against the provider before trusting
//! them for hard cost caps.

/// Per-million-token prices for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    /// USD per 1M input tokens.
    pub input_per_mtok: f64,
    /// USD per 1M output tokens.
    pub output_per_mtok: f64,
}

impl ModelPrice {
    const fn new(input_per_mtok: f64, output_per_mtok: f64) -> Self {
        Self {
            input_per_mtok,
            output_per_mtok,
        }
    }
}

/// Look up prices for a model id. Matching is by substring (case-insensitive) so
/// that dated or provider-prefixed ids — `claude-opus-4-8`,
/// `anthropic.claude-opus-4-8`, `claude-haiku-4-5-20251001` — all resolve to the
/// right family. Returns `None` for unknown models (the caller then leaves cost
/// unestimated rather than guessing).
pub fn price_of(model: &str) -> Option<ModelPrice> {
    let m = model.to_ascii_lowercase();

    // --- Anthropic (authoritative; per-MTok input/output) --------------------
    // Order matters: check the most specific fragments first so e.g. "opus-4-8"
    // isn't shadowed by a looser "opus" rule.
    if m.contains("fable-5") || m.contains("mythos-5") || m.contains("mythos-preview") {
        return Some(ModelPrice::new(10.00, 50.00));
    }
    if m.contains("opus") {
        // Opus 4.5 / 4.6 / 4.7 / 4.8 all price the same per MTok.
        return Some(ModelPrice::new(5.00, 25.00));
    }
    if m.contains("sonnet") {
        // Sonnet 4.5 / 4.6.
        return Some(ModelPrice::new(3.00, 15.00));
    }
    if m.contains("haiku") {
        return Some(ModelPrice::new(1.00, 5.00));
    }

    // --- Codex / OpenAI (best-effort — VERIFY before relying on for caps) -----
    // Codex reports tokens only, so we need *some* number to compute a cost. The
    // exact model id Codex emits can vary by version; treat these as estimates.
    if m.contains("gpt-5") || m.contains("o4") || m.contains("o3") {
        return Some(ModelPrice::new(1.25, 10.00));
    }
    if m.contains("gpt-4o-mini") {
        return Some(ModelPrice::new(0.15, 0.60));
    }
    if m.contains("gpt-4o") || m.contains("gpt-4.1") {
        return Some(ModelPrice::new(2.50, 10.00));
    }

    None
}

/// Compute the cost in USD for `tokens_in`/`tokens_out` on `model`, or `None`
/// if the model is unknown. This is the **fallback** path — when an agent
/// reports a cost directly, store that instead of calling this.
pub fn cost_of(model: &str, tokens_in: u32, tokens_out: u32) -> Option<f64> {
    let price = price_of(model)?;
    let input = (tokens_in as f64 / 1_000_000.0) * price.input_per_mtok;
    let output = (tokens_out as f64 / 1_000_000.0) * price.output_per_mtok;
    Some(input + output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_claude_families_by_substring() {
        assert_eq!(price_of("claude-opus-4-8"), Some(ModelPrice::new(5.00, 25.00)));
        // Provider-prefixed and dated ids still resolve.
        assert_eq!(
            price_of("anthropic.claude-opus-4-8"),
            Some(ModelPrice::new(5.00, 25.00))
        );
        assert_eq!(
            price_of("claude-haiku-4-5-20251001"),
            Some(ModelPrice::new(1.00, 5.00))
        );
        assert_eq!(price_of("claude-sonnet-4-6"), Some(ModelPrice::new(3.00, 15.00)));
        assert_eq!(price_of("claude-fable-5"), Some(ModelPrice::new(10.00, 50.00)));
    }

    #[test]
    fn unknown_model_has_no_price() {
        assert_eq!(price_of("totally-made-up"), None);
        assert_eq!(cost_of("totally-made-up", 1000, 1000), None);
    }

    #[test]
    fn computes_cost_from_tokens() {
        // 1M in + 1M out on Opus = $5 + $25 = $30.
        let cost = cost_of("claude-opus-4-8", 1_000_000, 1_000_000).unwrap();
        assert!((cost - 30.0).abs() < 1e-9, "got {cost}");

        // 500k in + 100k out on Haiku = $0.50 + $0.50 = $1.00.
        let cost = cost_of("claude-haiku-4-5", 500_000, 100_000).unwrap();
        assert!((cost - 1.0).abs() < 1e-9, "got {cost}");
    }
}
