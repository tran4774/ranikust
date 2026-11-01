#!/usr/bin/env bash
# The ultra benchmark: the e2e suite on nextjs-kaniko's W1–W3 (tens of
# minutes per round). Only when the maintainer asks for performance numbers
# or an acceptance run (docs/testing.md#which-test-for-what).
RANIKUST_E2E_PROFILE=ultra exec bash "$(dirname "$0")/e2e.sh" "$@"
