---
name: pdf-processing
description: Extract text and tables from PDF files, fill forms, and merge documents. Use when the user mentions PDFs, forms, or scanned documents.
license: Complete terms in LICENSE.txt
allowed-tools: Read, Write, Bash
metadata:
  category: document-processing
  version: 1.2.0
---

# PDF processing

## Quick start

For text extraction, prefer `pdftotext -layout` — it preserves the column
structure that a naive extractor destroys, and most downstream parsing depends
on that structure.

```bash
pdftotext -layout input.pdf -
```

## Forms

Filling a form means writing field values, not overlaying text. See
`references/forms.md` for the field-name conventions each producer uses.

## When not to use this

Scanned documents with no text layer need OCR first; this skill assumes a text
layer exists.
