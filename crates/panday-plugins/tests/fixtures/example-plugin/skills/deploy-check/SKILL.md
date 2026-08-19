---
name: deploy-check
description: Verify a deploy is safe — checks migrations, feature flags and the on-call rota.
triggers:
  - deploy
  - release
license: MIT
author: someone-else
---

# Deploy check

1. Confirm migrations are backwards compatible.
2. Confirm the feature flag defaults to off.
3. Confirm someone is on call.

See `references/checklist.md` for the long form.
