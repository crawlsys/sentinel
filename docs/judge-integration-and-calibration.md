# Judge Integration & Calibration

> Status: implemented. Judge enforcement is structural through the proof chain:
> `step_judge` records an independent verdict, then `submit_step_complete`
> refuses to seal a StepProof unless that verdict is sufficient.
> Evidence: `crates/sentinel-infrastructure/tests/live_judge_pressure.rs`.

## Enforcement Path

The proof chain is the enforcement substrate:

```text
agent calls mcp__skills__<skill>__step_<n>
  -> step_gate: prior StepProof exists?
  -> tool executes
  -> step_judge on PostToolUse: gather evidence, call JudgeService, record verdict
  -> submit_step_complete: require that independent verdict, seal StepProof, seal LangGraph checkpoint
  -> next step_gate reads the sealed proof
```

`PostToolUse` itself never blocks. That layer observes completed work and writes
the verdict. The blocking point is `submit_step_complete`, because it owns proof
sealing and the LangGraph checkpoint boundary.

## Mandatory Behavior

- `step_judge` is registered on `PostToolUse` for the
  `mcp__skills__<skill>__step_<id>` namespace.
- `submit_step_complete` rejects missing independent judge verdicts.
- `submit_step_complete` rejects non-sufficient independent judge verdicts.
- Caller-supplied reasoning is retained only as context; it cannot replace the
  independent verdict.
- Terminal step status writes must use `sentinel__submit_step_complete`, not
  `sentinel__update_step`, so StepProof and LangGraph checkpoint evidence are
  committed together.
- There is no runtime judge-enforcement mode flag. Enforcement is the production
  path.

## Calibration Standard

The live pressure test remains the calibration gate:

- `genuinely-sufficient` must pass when the evidence contains named passing
  tests, relevant diffs, reproduction output, or equivalent concrete proof.
- `bare-claim-no-proof` must fail.
- `prompt-injection-in-evidence` must fail and must not echo attacker text.
- `subtle-insufficiency-vacuous-test` stays diagnostic so borderline behavior
  remains visible without redefining the hard gate.

Prompt calibration should stay skeptical of unsupported claims while accepting
work that is actually demonstrated. The target is not harsher judging; the
target is correct judging tied to evidence.

## Operational Checks

- Hook registration: `config/hooks.toml`, hook id `step-judge`.
- Hook runtime path: `crates/sentinel-cli/src/hook_cmd.rs`.
- Verdict producer: `crates/sentinel-application/src/hooks/step_judge.rs`.
- Proof seal gate: `crates/sentinel-application/src/mcp_handler.rs`.
- Proof engine refusal path:
  `crates/sentinel-application/src/proof_engine.rs`.

Useful verification:

```bash
cargo test -p sentinel --test e2e_judge_integration --quiet
cargo test -p sentinel-application submit_step_complete --quiet
cargo test -p sentinel-infrastructure live_judge_pressure -- --ignored
```
