"""json-bench through the gateway (docs/19 M19.1).

The Rust suite (`panday_harness::json_bench`) owns the corpus and the scoring; this is the
`inspect-ai` front end for the same question, because docs/19 asks for inspect-ai specifically and
because it is what gives us log viewing, retries and parallelism for free.

Two rules this file follows:

1. **It points at the gateway, never at a provider.** An eval that called Anthropic directly would
   measure a model we do not ship, in a configuration no user has.
2. **It fails loudly when unconfigured.** A suite that quietly falls back to a default endpoint
   produces a number for something other than what the operator meant to measure.
"""

from __future__ import annotations

import json
import os

from inspect_ai import Task, task
from inspect_ai.dataset import Sample
from inspect_ai.scorer import CORRECT, INCORRECT, Score, Target, accuracy, scorer
from inspect_ai.solver import generate


SHAPES: list[tuple[str, dict, str]] = [
    (
        "flat-strings",
        {
            "type": "object",
            "required": ["name", "language"],
            "properties": {"name": {"type": "string"}, "language": {"type": "string"}},
        },
        "the crate `panday-gateway` and the language it is written in",
    ),
    (
        "enum",
        {"type": "object", "required": ["severity"], "properties": {"severity": {"enum": ["low", "medium", "high"]}}},
        "a bug of medium severity",
    ),
    (
        "nested-object",
        {
            "type": "object",
            "required": ["tool", "args"],
            "properties": {
                "tool": {"type": "string"},
                "args": {
                    "type": "object",
                    "required": ["path"],
                    "properties": {"path": {"type": "string"}, "limit": {"type": "integer"}},
                },
            },
        },
        "a call to the `read` tool for src/lib.rs limited to 50 lines",
    ),
]

PHRASINGS = [
    "Describe {subject}.",
    "{subject} — as JSON.",
    "Describe {subject} and explain your reasoning.",
    "Describe {subject}. No prose.",
]


def base_url() -> str:
    url = os.environ.get("PANDAY_GATEWAY_URL") or os.environ.get("PANDAY_BASE_URL")
    if not url:
        raise SystemExit(
            "PANDAY_GATEWAY_URL is not set (PANDAY_BASE_URL is accepted as a fallback). "
            "This eval measures models *through the gateway*; "
            "falling back to a provider default would measure something else entirely."
        )
    # inspect-ai's openai provider reads these; setting them here keeps the failure above as the
    # single place configuration is checked.
    os.environ.setdefault("OPENAI_BASE_URL", f"{url.rstrip('/')}/v1")
    os.environ.setdefault("OPENAI_API_KEY", os.environ.get("PANDAY_API_KEY", "unset"))
    return url


def unfence(text: str) -> str:
    """Strip a markdown fence, exactly as the harness does before parsing.

    Anything beyond this — hunting for an object inside prose — would measure our salvage code
    rather than the model.
    """
    stripped = text.strip()
    if not stripped.startswith("```"):
        return stripped
    body = stripped.split("\n", 1)[1] if "\n" in stripped else stripped
    return body.rsplit("```", 1)[0].strip() if "```" in body else body.strip()


def satisfies(schema: dict, value: object, path: str = "") -> str | None:
    """Return None if `value` satisfies `schema`, or a one-line reason if not.

    The same subset the Rust validator checks (`panday_harness::json_schema`), and for the same
    reason: a checker whose failure messages a person can act on beats a spec-compliant one whose
    messages are paths.
    """
    if "enum" in schema:
        return None if value in schema["enum"] else f"{path or 'the root'} is not one of {schema['enum']}"

    kind = schema.get("type")
    if kind == "object":
        if not isinstance(value, dict):
            return f"{path or 'the root'} must be an object"
        for field in schema.get("required", []):
            if field not in value:
                return f"missing required field `{'.'.join(filter(None, [path, field]))}`"
        for field, sub in schema.get("properties", {}).items():
            if field in value:
                failure = satisfies(sub, value[field], ".".join(filter(None, [path, field])))
                if failure:
                    return failure
        return None
    if kind == "array":
        if not isinstance(value, list):
            return f"{path or 'the root'} must be an array"
        for i, item in enumerate(value):
            failure = satisfies(schema.get("items", {}), item, f"{path}[{i}]")
            if failure:
                return failure
        return None
    if kind == "string" and not isinstance(value, str):
        return f"{path or 'the root'} must be a string"
    if kind == "integer" and not (isinstance(value, int) and not isinstance(value, bool)):
        return f"{path or 'the root'} must be an integer"
    if kind == "boolean" and not isinstance(value, bool):
        return f"{path or 'the root'} must be a boolean"
    return None


@scorer(metrics=[accuracy()])
def schema_valid():
    async def score(state, target: Target) -> Score:
        schema = json.loads(target.text)
        text = unfence(state.output.completion)
        try:
            parsed = json.loads(text)
        except json.JSONDecodeError:
            # Distinct from a shape failure: "the model chats at you" and "the model gets the shape
            # wrong" need different fixes.
            return Score(value=INCORRECT, explanation="not JSON")
        failure = satisfies(schema, parsed)
        if failure:
            return Score(value=INCORRECT, explanation=f"wrong shape: {failure}")
        return Score(value=CORRECT)

    return score


@task
def json_discipline() -> Task:
    base_url()
    samples = [
        Sample(
            input=(
                f"{phrasing.format(subject=subject)}\n\n"
                f"Reply with JSON only, matching this schema:\n{json.dumps(schema)}"
            ),
            target=json.dumps(schema),
            id=f"{shape}/{i:02}",
        )
        for shape, schema, subject in SHAPES
        for i, phrasing in enumerate(PHRASINGS)
    ]
    return Task(dataset=samples, solver=generate(), scorer=schema_valid())
