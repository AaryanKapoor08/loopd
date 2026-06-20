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
    // `exec --json` stream carries no model id, so the Codex adapter stamps its
    // default (`gpt-5-codex`), which matches the `gpt-5` fragment below. The exact
    // model id Codex emits can vary by version; treat these as estimates.
    if m.contains("gpt-5") || m.contains("o4") || m.contains("o3") {
        // gpt-5 / gpt-5-codex: $1.25/MTok input, $10/MTok output.
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

/// Cache-token price multipliers, relative to the base input rate (Anthropic
/// pricing reference). A cache *write* (`cache_creation_input_tokens`) costs
/// ~1.25× input at the 5-minute TTL (2× at 1h — we assume the 5m default, which
/// is what CC/transcript usage reports for the bare field); a cache *read*
/// (`cache_read_input_tokens`) costs ~0.10× input. Counting cache tokens at the
/// full input rate would over-bill; ignoring them undercounts badly.
const CACHE_WRITE_MULT: f64 = 1.25;
const CACHE_READ_MULT: f64 = 0.10;

/// A token-usage block as an agent reports it (CC `message.usage`, the Mode-B
/// transcript, the SDK). Cache tokens are reported in **separate buckets** on
/// the wire; capturing them here is what lets totals and cost include them — a
/// long cached session is mostly cache reads, so summing only `input_tokens`
/// undercounts both (vibe-kanban `claude.rs`; ARCHITECTURE.md §4). The Phase-3
/// parser builds one of these per turn, stores [`Usage::total_input`] in
/// `Run.tokens_in`, and (when the adapter doesn't self-report cost) prices it
/// via [`cost_of_usage`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// Fresh (uncached) input tokens.
    pub input_tokens: u32,
    /// Tokens written to the prompt cache this turn (~1.25× input price).
    pub cache_creation_input_tokens: u32,
    /// Tokens served from the prompt cache this turn (~0.10× input price).
    pub cache_read_input_tokens: u32,
    /// Output tokens generated this turn.
    pub output_tokens: u32,
}

impl Usage {
    /// Effective input tokens = fresh input + cache writes + cache reads. This
    /// is the number that flows to `Run.tokens_in` and the cost cap; counting
    /// only `input_tokens` undercounts because cache reads dominate long runs.
    pub fn total_input(&self) -> u32 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }
}

/// Compute the fallback cost in USD for a full [`Usage`] block on `model`, or
/// `None` if the model is unknown. Cache tokens are priced at their real
/// multipliers (write 1.25×, read 0.10× of the input rate) rather than lumped
/// in at full input price. This is the **fallback** path — when an agent reports
/// a cost directly (CC `total_cost_usd`), store that instead of calling this.
pub fn cost_of_usage(model: &str, usage: &Usage) -> Option<f64> {
    let price = price_of(model)?;
    const MTOK: f64 = 1_000_000.0;
    let input = (usage.input_tokens as f64 / MTOK) * price.input_per_mtok;
    let cache_write =
        (usage.cache_creation_input_tokens as f64 / MTOK) * price.input_per_mtok * CACHE_WRITE_MULT;
    let cache_read =
        (usage.cache_read_input_tokens as f64 / MTOK) * price.input_per_mtok * CACHE_READ_MULT;
    let output = (usage.output_tokens as f64 / MTOK) * price.output_per_mtok;
    Some(input + cache_write + cache_read + output)
}

/// Compute the cost in USD for plain `tokens_in`/`tokens_out` on `model`, or
/// `None` if the model is unknown. Thin wrapper over [`cost_of_usage`] with
/// empty cache buckets — for callers (e.g. Codex `turn.completed`) that report a
/// single input count with no cache breakdown. One cost path, no drift.
pub fn cost_of(model: &str, tokens_in: u32, tokens_out: u32) -> Option<f64> {
    cost_of_usage(
        model,
        &Usage {
            input_tokens: tokens_in,
            output_tokens: tokens_out,
            ..Usage::default()
        },
    )
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
    fn codex_default_model_resolves_and_computes_cost() {
        // The Codex adapter stamps `gpt-5-codex` (the stream carries no model);
        // it must resolve so the Phase-8 gate "cost computed from tokens, not
        // blank" holds. A real captured Codex usage block → non-zero cost.
        assert_eq!(price_of("gpt-5-codex"), Some(ModelPrice::new(1.25, 10.00)));
        let usage = Usage {
            input_tokens: 75_389 - 39_168, // fresh = total - cached subset
            cache_read_input_tokens: 39_168,
            output_tokens: 109 + 57, // output + reasoning
            ..Usage::default()
        };
        assert_eq!(usage.total_input(), 75_389); // re-sums to the wire total
        let cost = cost_of_usage("gpt-5-codex", &usage).expect("Codex cost must compute");
        assert!(cost > 0.0, "Codex cost must not be blank: {cost}");
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

    #[test]
    fn usage_total_and_cost_include_cache_tokens() {
        // A cache-heavy turn: most of the input is served from cache, as in a
        // long session. Counting only `input_tokens` would miss ~99% of it.
        let usage = Usage {
            input_tokens: 1_000,
            cache_creation_input_tokens: 10_000,
            cache_read_input_tokens: 100_000,
            output_tokens: 500,
        };

        // The displayed total must count every input bucket, not just fresh input.
        assert_eq!(usage.total_input(), 111_000);

        // Opus: $5/MTok in, $25/MTok out; cache write 1.25×, cache read 0.10×.
        //   input       1_000/1e6 * 5            = 0.005
        //   cache_write 10_000/1e6 * 5 * 1.25    = 0.0625
        //   cache_read  100_000/1e6 * 5 * 0.10   = 0.05
        //   output      500/1e6 * 25             = 0.0125
        //   total                                 = 0.13
        let cost = cost_of_usage("claude-opus-4-8", &usage).unwrap();
        assert!((cost - 0.13).abs() < 1e-9, "got {cost}");

        // The bug this guards against: pricing only `input_tokens` undercounts.
        let naive = cost_of("claude-opus-4-8", usage.input_tokens, usage.output_tokens).unwrap();
        assert!(naive < cost, "cache tokens must raise the bill: naive {naive} vs {cost}");

        // Unknown models stay unpriced through the usage path too.
        assert_eq!(cost_of_usage("totally-made-up", &usage), None);
    }
}
