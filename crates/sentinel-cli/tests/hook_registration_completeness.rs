//! Guard against built-but-unwired hooks.
//!
//! Two milestone features (`step_anomaly`, `spec_challenge_gate`) shipped
//! registered in `HOOK_NAMES` + fully implemented but with **no dispatch
//! call-site** in `hook_cmd.rs` — so they never fired until a reality-check
//! audit found them. This test makes that failure mode impossible to repeat:
//! every `HOOK_NAMES` entry must either be dispatched from the hook command
//! dispatcher OR be explicitly classified as a non-dispatched helper.
//!
//! Adding a new hook to `HOOK_NAMES` without wiring it into `hook_cmd.rs`
//! (and without justifying it as a helper) now fails CI.

use sentinel_application::hooks::HOOK_NAMES;

/// Entries that live in `HOOK_NAMES` but are intentionally NOT dispatched
/// from `hook_cmd.rs` — they're shared helper modules other hooks call, not
/// standalone dispatched hooks. Keep this list tiny and justified; a new
/// entry here is a deliberate "this is a helper, not a dormant hook" claim.
const KNOWN_NON_DISPATCHED_HELPERS: &[&str] = &[];

/// The hook command dispatcher source. Embedded at compile time so the test
/// reads exactly the dispatcher that ships, not a runtime-resolved path.
const HOOK_CMD_SRC: &str = include_str!("../src/hook_cmd.rs");

#[test]
fn every_registered_hook_is_dispatched_or_a_known_helper() {
    let mut dormant: Vec<&str> = Vec::new();

    for &hook in HOOK_NAMES {
        if KNOWN_NON_DISPATCHED_HELPERS.contains(&hook) {
            continue;
        }
        // A hook is "dispatched" if the dispatcher references it either via a
        // module path (`hooks::<name>::`) or via the telemetry context
        // (`mk_ctx("<name>")`) — both forms appear at real call-sites.
        let module_call = format!("hooks::{hook}::");
        let metrics_ctx = format!("mk_ctx(\"{hook}\")");
        let dispatched =
            HOOK_CMD_SRC.contains(&module_call) || HOOK_CMD_SRC.contains(&metrics_ctx);
        if !dispatched {
            dormant.push(hook);
        }
    }

    assert!(
        dormant.is_empty(),
        "These hooks are registered in HOOK_NAMES but never dispatched in \
         hook_cmd.rs (built-but-dormant). Wire them into the dispatcher, or — \
         if one is a shared helper with no process() entry point — add it to \
         KNOWN_NON_DISPATCHED_HELPERS with a justification: {dormant:?}"
    );
}

#[test]
fn helper_allowlist_entries_are_actually_registered() {
    // Don't let the allowlist rot: every helper we exempt must still be a
    // real HOOK_NAMES entry, otherwise the exemption is dead.
    for &helper in KNOWN_NON_DISPATCHED_HELPERS {
        assert!(
            HOOK_NAMES.contains(&helper),
            "KNOWN_NON_DISPATCHED_HELPERS lists '{helper}' but it is not in \
             HOOK_NAMES — remove the stale exemption"
        );
    }
}
