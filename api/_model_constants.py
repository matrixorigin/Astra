"""Shared constants for ORM models, resolved at import time from env vars."""

import os

EMBEDDING_DIM = int(os.environ.get("EMBEDDING_DIM", "384"))
