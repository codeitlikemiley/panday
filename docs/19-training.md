# 19 — Models, Evals & Training

> Researched in depth, August 2026. Prices and model names drift weekly;
> the *structure* of the decisions is what this doc pins down. Rule zero,
> from ADR-011: **evals before training, and no path that can't hand us
> weights** — the offline tier must inherit every model we make.

## 19.0 The landscape, compressed

Three facts shape everything:

1. **Closed-provider fine-tuning is dying.** OpenAI is winding down
   self-serve fine-tuning (closed to new users; RFT was o4-mini-only);
   Anthropic offers none to normal customers; Mistral deprecated theirs.
   Azure/Bedrock/Vertex-Gemini tune variants exist but **never export
   weights** — dead ends for us by rule zero.
2. **Open-model tuning matured.** Unsloth (solo-GPU king), Axolotl
   (config/multi-GPU), TRL+PEFT (reference layer), LLaMA-Factory (GUI),
   ms-swift (Qwen-family depth) are all healthy. torchtune is dead
   (maintenance-only since mid-2025; successor torchforge is research-grade).
   Rust-native training (burn/candle-lora) remains non-viable for LLM
   fine-tuning — train in Python, serve in Rust (ADR-001).
3. **Managed-with-export exists in three lanes**: HF Jobs (run TRL/Unsloth
   scripts on managed GPUs, artifacts to your Hub repo — cheapest on-ramp),
   Together (LoRA/full/DPO, checkpoint download explicitly supported,
   ~$0.48/M train tokens), and **Tinker** (Thinking Machines: managed
   LoRA SFT/RL on big open MoE with checkpoint-download API — the sleeper
   option for RL at scale without owning infra). Fireworks tunes well but
   verify weight export before depending on it. OpenPipe's SaaS became
   CoreWeave's W&B Training; its **ART** library (agent GRPO) lives on as
   OSS. Predibase went to Rubrik (enterprise tilt; skip).

## 19.1 Evals first (the part most people skip)

Infrastructure before any training run:

- **Harnesses**: `inspect-ai` (UK AISI) for agentic/tool-use evals — sandboxed
  scoring fits us natively; `lighteval` (HF) for standard benchmarks; both
  run via `training/evals/` and hit models **through panday-gateway** (so
  evals exercise routing, caching, and adapters — three birds).
- **Suites we own** (versioned in-repo, grown from real transcripts):
  `route-bench` (task-classification accuracy), `reduce-bench`
  (reduce-then-solve success, 15.M5), `agent-bench` (N repo tasks with
  verifiable rewards: tests pass), `json-bench` (schema-validity rates),
  plus per-model `capability profiles` for the local catalog (18).
- **Gates**: a model (base or tuned, cloud or GGUF) enters routing pools only
  with a scorecard; a tuned model ships only if it beats the incumbent on its
  target suite **after quantization** (quantize → then eval; quality dies at
  the quant step, so that's where you measure).
- Decontamination discipline: every training set is n-gram + semantic
  (semhash) deduped against every gate suite. Contaminated evals are worse
  than no evals.

## 19.2 The method menu (when to reach for what)

| Method | Use when | Data | Cost vs QLoRA-SFT |
|---|---|---|---|
| QLoRA (4-bit base + LoRA) | **default for everything** | 500–50k examples | 1x baseline (a 7–14B run ≈ $2–5 rented) |
| LoRA (16-bit base) | quality-critical final runs | same | ~1–2x, needs more VRAM |
| Full SFT | tiny models (<4B) or deep behavior shift | 10k–1M | 3–10x; rarely justified (well-configured LoRA matches full FT in typical post-training regimes) |
| DPO / ORPO / KTO | polish after SFT: format discipline, tone, pick-the-better-output | 1–10k pairs (KTO: binary 👍/👎 — minable from product telemetry) | ~1–2x |
| GRPO / agentic RL | you have a **checkable reward** (tests pass, JSON valid, route correct) and prompts, not gold outputs | 100–2k tasks | 3–20x (rollouts dominate; needs colocated vLLM) |
| Distillation (frontier→small) | building any task specialist | teacher-generated | teacher API cost dominates |
| Continued pretrain | new language/domain corpus | 100M+ tokens | 10–100x; we don't need it |
| Embedding fine-tune | retrieval, semantic routing fallback | hundreds–few-k pairs | minutes on any GPU |
| Encoder classifier head | **fixed-label routing** | 1–5k labels | ~$2/run; the right tool (a decoder LLM is the wrong one) |

**GRPO tooling**: Unsloth GRPO (cheapest single-GPU entry), TRL
GRPOTrainer/AsyncGRPO, **ART** (OpenPipe, agent-first with RULER
auto-rewards), veRL/prime-rl (scale), or Tinker (managed). Start with
Unsloth/ART; graduate only if scale forces it.

**Distillation legality** (researched, but treat as interpretation — verify
current terms and get counsel before shipping a trained artifact):
OpenAI/Google terms bar training *competing* models on their outputs.
Anthropic's support-center guidance ("Can I use my outputs to train an AI
model") describes training specialized, non-competing tools (classifiers,
summarizers, extractors) on Claude outputs as permitted while prohibiting
competing general models — our router and summarizer appear to qualify, but
that is guidance-level text, not a negotiated license; confirm against the
commercial terms in force when you train.
For the coding-agent specialist, use **open-weight teachers**
(DeepSeek-V4, Kimi K3, Qwen3.x — permissive licenses that allow distillation)
to stay unambiguous. Transcript mining inherits the same analysis, plus
consent: explicit ToS opt-in, enterprise/DPA tenants excluded, PII scrubbed
(Presidio/GLiNER-class) before storage, per-example provenance logged.

## 19.3 Hardware lanes (Aug 2026 spot prices; ±30% is weather)

| Lane | Cost | Good for |
|---|---|---|
| Rented consumer GPU (RunPod/Vast): 4090 $0.33–0.45/hr, 5090 ~$0.69/hr | ~$1–3 per QLoRA iteration of 2–14B | the default lane |
| Rented datacenter: A100-80 $1.1–1.9/hr, H100 $1.7–3.3/hr, H200 ~$3.6/hr, B200 ~$5–6.5/hr | SFT 14–32B: $10–40/run; GRPO 1–3 days × 2–4 GPUs: $200–1,500/run | specialist training |
| Modal (serverless, per-second: H100 ≈$3.95/hr, A100 ≈$2.50/hr) | automated nightly retrains from CI | the pipeline lane |
| HF Jobs | T4→H100 managed, artifacts to your repo | cheapest managed on-ramp |
| Owned: used 3090 24GB ($600–900, the value king) / 5090 32GB | 7–14B QLoRA comfortable; 27–32B tight | iteration without a meter running |
| Apple Silicon (MLX): 64GB→14B LoRA/32B QLoRA; 128GB→~30B practical ceiling | ~5–10x slower than a 4090 | overnight 2–9B jobs on hardware you have |

Full-SFT of a 7–8B (~60M tokens): ~$15–40 on 1×H100 vs ~$2–4 QLoRA on a
4090 — iteration money goes to *data and evals*, not weights.

## 19.4 Export & serve pipeline (every run ends here)

```
train (Python: Unsloth/TRL/ART) → safetensors adapter
  ├─ adapter lane:  convert_lora_to_gguf.py → llama-server --lora (hot-swap; multi-specialist tier)
  └─ ship lane:     merge into BF16 base → convert_hf_to_gguf.py
                    → llama-imatrix over OUR domain text (real tool outputs/transcripts)
                    → llama-quantize Q4_K_M / IQ4_XS
                    → eval gate (19.1, post-quant) → signed catalog entry (18)
Cloud serving of tuned families: vLLM --enable-lora (dozens of adapters over one base)
Speedup, later: EAGLE-3 draft via vLLM speculators (2–4x decode), or simply a
small same-family draft model for llama.cpp --model-draft.
```

Never merge into a 4-bit base; merge BF16 then quantize. Unsloth's
`save_pretrained_gguf` collapses the ship lane to one call.

## 19.5 The first three models (concrete, budgeted)

**Model 1 — task-router classifier.** NOT a decoder LLM: fine-tune
**ModernBERT-base (~150M)** with a classification head on 3–5k labeled
requests (labels distilled from Claude — a non-competing classifier per the
guidance discussed in 19.2, verify terms first; 10% hand-verified). Trains in ~20 min on anything. Serve in-process
(ONNX/candle) at ms latency behind the `Classifier` trait (12.M5), with an
embedding-similarity fallback for unseen classes.
**Cost: <$2/iteration, $20–100 total.** This is deliberately the first
artifact: it teaches the whole pipeline end to end in a weekend.

**Model 2 — tool-output summarizer (the reducer's semantic tier, 15.M6).**
Base **Qwen3.5-2B or 4B** (Apache). Distill 20–50k
(raw tool output → ideal digest) pairs from your own production reducer
traces + teacher passes; QLoRA r32–64 with Unsloth on a rented 4090
(1–3 hr/run); optional 2–5k-pair DPO for format discipline. Ship merged
GGUF Q4_K_M with imatrix over real tool outputs (~3GB at 4B — a perfect
offline artifact).
**Cost: $1–3/iteration, $150–400 total to first useful.** Highest ROI custom
model in the plan: it directly cuts COGS every session.

**Model 3 — coding-agent specialist (last; 10x the money and risk).**
Base: Qwen3.6-27B-dense or Devstral-24B class (trainability), or a
30-80B-A3B MoE for cheap inference. Stage 1: SFT-QLoRA r64–128 on 5–20k
curated *successful* agent trajectories (open-weight teachers to stay
ToS-clean). Stage 2: **GRPO on 200–1k verifiable repo tasks** (reward = tests
pass, in our own T3 sandboxes — the eval infra doubles as the RL environment)
via ART on 2–4×H100/H200, or Tinker if managed. Gate on agent-bench vs the
workhorse pool it would displace.
**Cost: $10–40/SFT iteration; $200–1,500/GRPO run; ~$1–5k total** to a model
that earns a routing pool slot. Phase-5 exit criterion (00): ≥30% of routed
traffic at equal-or-better evals and lower cost.

## 19.6 Data pipeline (`training/` layout)

```
training/                    # Python uv project — the ONE Python island (ADR-001)
├── datasets/                # builders: transcript mining, distillation, synthetic (distilabel)
├── evals/                   # inspect-ai + lighteval suites; scorecard emitters
├── recipes/                 # unsloth/trl/art configs per model (yaml + py)
├── export/                  # merge, GGUF convert, imatrix, quantize, sign
└── registry.py              # artifact registry client → catalog entries (18)
```

Format: OpenAI-style `messages` JSONL everywhere (tool-call turns preserved
natively — never flattened to text). Dataset builders emit provenance +
license fields per example; `semhash` dedup + n-gram decontam runs in the
builder, not as an afterthought. Orchestration: Rust `panday-models` CLI (or
just `just` recipes at first) submits jobs to Modal/HF Jobs and pulls
artifacts; CI runs eval gates nightly.

## Milestones

- **M19.1** Eval spine: inspect-ai through the gateway; route-bench + json-bench with 200 fixtures each; scorecard artifact format. ✅ *(shipped: `panday_types::scorecard` + `proto/scorecard.schema.json`, `panday_harness::json_bench` (200 fixtures) and `panday_harness::json_schema`, `cargo xtask json-bench` / `just bench`, `training/evals/json_discipline.py`, route-bench grown to 50 and emitting the same artifact. **Two clauses left open** — see below.)*

  **The scorecard is JSON, and markdown is a rendering of it.** A gate is only a gate if something
  can read it without a human, and a gate that parses a table out of prose breaks the first time
  somebody improves the wording. The type carries no clock — `at` is passed in — because a
  scorecard that timestamped itself would diff on every regeneration, and a file that always diffs
  is one nobody reviews. An empty run scores **zero**, not one: "nothing ran" must never read as
  "everything passed", which is the failure mode of every gate that divides by a count.

  **json-bench scores three failure modes apart**, because their fixes differ: not JSON at all
  (prose, or an apology), JSON of the wrong shape (the interesting one), and a call that failed.
  The last is counted as a failed case rather than skipped — a suite that drops its errors reports
  a rate for the cases that happened to work, which is the most flattering possible lie. A fenced
  answer is unwrapped, because the real harness unwraps one; prose *around* JSON is not salvaged,
  because that would measure our salvage code rather than the model.

  **How 200 fixtures is reached, said out loud:** ten shapes × twenty phrasings. Both dimensions
  matter — a model that handles nested objects but only when asked politely is not usable — and
  the alternative was writing two hundred near-duplicates by hand. Mined transcripts (M19.4) will
  replace the phrasing dimension with real ones.

  **The validator is a subset, and the corpus is asserted against it.** An ignored constraint is a
  case scored as passing, so a test walks every schema in the corpus and fails if it uses a keyword
  the validator does not check. Failure messages are written for a person reading a scorecard:
  "missing required field `args.path`" is a bug report, `#/properties/args: does not match` is a
  puzzle.

  **Doubling route-bench found real bugs, which is the point of the milestone.** The corpus went
  from 24 hand-labelled cases to 50, and the same classifier that scored 92% scored **66% with two
  confidently-wrong answers**. Three genuine gaps: routing questions had no markers and fell
  through to `Code`; most summarise and extract asks never contain the words "summarise" or
  "extract"; and a single weak marker (bare `test`, `build`) was enough to clear the trust gate, so
  "the driving test is on tuesday" was confidently code. Fixed, the classifier scores **94%
  (47/50), zero confidently wrong** on the harder corpus. A suite you pass is not evidence until it
  is a suite that could have failed.

  **Left open, deliberately, and both need hardware this repo does not have:**

  - *A measured number for json-bench.* The suite runs end to end against a gateway; nothing has
    run it against a real model, so no scorecard is committed. `cargo xtask json-bench --write`
    produces one, and it records the quantization it measured at (docs/19 §gates: quantize → then
    eval).
  - *route-bench at 200 fixtures.* It is at 50. The remaining 150 should come from mined traffic
    (M19.4), not from invention: a corpus of made-up prompts at that size measures our imagination,
    and the 24-case version already demonstrated what that costs.

  **inspect-ai points at the gateway, never at a provider** (`training/evals/json_discipline.py`),
  and refuses to start without `PANDAY_BASE_URL` rather than falling back to a provider default —
  an eval that quietly measured something else is worse than one that did not run. Nothing in
  `training/` runs in CI beyond a syntax check, and that is stated in its README: a green tick that
  means "we did not measure" is how an eval suite rots.
- **M19.2** Capability profiles for 3 local catalog models, generated not hand-written.
- **M19.3** Model 1 shipped: classifier behind `Classifier` trait beats heuristic on route-bench by ≥10pt; deployed in shadow, then live.
- **M19.4** Transcript mining pipeline with consent flags + PII scrub + provenance; first 10k-pair summarizer dataset. ✅ *(shipped: `panday_harness::mining`, `cargo xtask mine --logs <dir> --out <file>`. **The 10k-pair dataset is not here** — it needs 10k consented transcripts, and this repo has none.)*

  **Consent, then scrub, then provenance — and each step defaults to refusing.** The mistakes in a
  data pipeline are the unrecoverable kind: a customer's secret that reaches a dataset is in every
  checkpoint trained on it, and a licence you did not have is one you cannot retroactively obtain.

  **`Unknown` is a distinct state from `Denied`.** A session nobody asked about is not mined, and
  the pipeline never infers consent from a plan, an account type, or where a log happened to be
  stored — a directory move must not become a permission. The `--consent` flag on the miner defaults
  to unknown, which mines nothing, so the assertion is always a person's.

  **Scrubbing is layered, and the last gate drops rather than fixes.** Vault-known values first,
  then shapes that are credentials by construction — keys by prefix or by length-and-entropy,
  emails, IPs, home directories that name a person. Anything that still trips the detector after
  scrubbing drops the example: a shape the scrubber did not recognise is a shape we do not
  understand, and a dataset is worth less than a leak costs. A test asserts ordinary output survives
  intact, because a scrubber that mangles normal text produces a corpus that teaches mangling.

  **Provenance travels with every row**, including `scrubber_version`. When a scrubber bug is found
  — and one will be — "which examples came out of the broken version" has to have an answer that is
  not "all of them".

  **The first pairs are the reducer's own judgements.** A long tool output and the reduction that
  stood in for it is exactly the task a small summarizer should learn, and the reducer has already
  made that call thousands of times per session. A reduction that removed nothing is skipped: a
  model trained on copies learns to copy.

  **What is missing is the data, not the pipeline.** 10k pairs needs 10k consented sessions. The
  miner runs end to end today over a directory of logs and reports exactly why each candidate was
  dropped — which is the part that had to exist before any transcript was worth collecting.
- **M19.5** Model 2 shipped: reduce-bench regression zero, ≥25% cheaper semantic tier than the provider cheap-pool it replaces; GGUF in catalog.
- **M19.6** agent-bench (50 verifiable repo tasks in T3) doubling as GRPO environment.
- **M19.7** Model 3 v1: SFT stage beats base on agent-bench; go/no-go review for the GRPO spend.
