"""Unit tests: _rewrite_tool_call_ids for strict_tool_call_ids quirk.

Verifies:
- Standard "call_xxx" ids are left unchanged
- Non-standard ids (e.g. "read_file:1") are rewritten to "call_<uuid>"
- Assistant tool_calls and tool messages stay consistent (same mapping)
- Empty/missing ids are left as-is
- Non-tool messages (user, system) are passed through unchanged
- Multiple occurrences of the same non-standard id get the same replacement
"""

import re

from core.llm.client import _rewrite_tool_call_ids

_CALL_PATTERN = re.compile(r"^call_[a-zA-Z0-9]+$")


class TestRewriteToolCallIds:
    def test_standard_ids_unchanged(self):
        """IDs matching 'call_xxx' must not be rewritten."""
        messages = [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_abc123",
                        "type": "function",
                        "function": {"name": "fn", "arguments": "{}"},
                    },
                ],
            },
            {"role": "tool", "tool_call_id": "call_abc123", "content": "result"},
        ]
        result = _rewrite_tool_call_ids(messages)
        assert result[0]["tool_calls"][0]["id"] == "call_abc123"
        assert result[1]["tool_call_id"] == "call_abc123"

    def test_nonstandard_ids_rewritten(self):
        """IDs like 'read_file:1' must be rewritten to 'call_<uuid>'."""
        messages = [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "read_file:1",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{}"},
                    },
                ],
            },
            {"role": "tool", "tool_call_id": "read_file:1", "content": "file contents"},
        ]
        result = _rewrite_tool_call_ids(messages)
        new_id = result[0]["tool_calls"][0]["id"]
        assert _CALL_PATTERN.match(new_id), f"Expected call_xxx format, got: {new_id}"
        assert new_id != "read_file:1"
        # Tool message must use the same rewritten id
        assert result[1]["tool_call_id"] == new_id

    def test_consistent_mapping_across_messages(self):
        """Same non-standard id appearing multiple times must map to the same replacement."""
        messages = [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "fn:0",
                        "type": "function",
                        "function": {"name": "fn", "arguments": "{}"},
                    },
                ],
            },
            {"role": "tool", "tool_call_id": "fn:0", "content": "r1"},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "fn:0",
                        "type": "function",
                        "function": {"name": "fn", "arguments": "{}"},
                    },
                ],
            },
            {"role": "tool", "tool_call_id": "fn:0", "content": "r2"},
        ]
        result = _rewrite_tool_call_ids(messages)
        ids = [
            result[0]["tool_calls"][0]["id"],
            result[1]["tool_call_id"],
            result[2]["tool_calls"][0]["id"],
            result[3]["tool_call_id"],
        ]
        assert len(set(ids)) == 1, f"All occurrences must map to same id, got: {ids}"

    def test_different_nonstandard_ids_get_different_replacements(self):
        """Different non-standard ids must get different replacements."""
        messages = [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "fn_a:0",
                        "type": "function",
                        "function": {"name": "a", "arguments": "{}"},
                    },
                    {
                        "id": "fn_b:1",
                        "type": "function",
                        "function": {"name": "b", "arguments": "{}"},
                    },
                ],
            },
            {"role": "tool", "tool_call_id": "fn_a:0", "content": "r1"},
            {"role": "tool", "tool_call_id": "fn_b:1", "content": "r2"},
        ]
        result = _rewrite_tool_call_ids(messages)
        id_a = result[0]["tool_calls"][0]["id"]
        id_b = result[0]["tool_calls"][1]["id"]
        assert id_a != id_b

    def test_empty_id_left_as_is(self):
        """Empty string id must not be rewritten."""
        messages = [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"id": "", "type": "function", "function": {"name": "fn", "arguments": "{}"}},
                ],
            },
        ]
        result = _rewrite_tool_call_ids(messages)
        assert result[0]["tool_calls"][0]["id"] == ""

    def test_non_tool_messages_unchanged(self):
        """User, system, and plain assistant messages must pass through unchanged."""
        messages = [
            {"role": "system", "content": "you are helpful"},
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi there"},
        ]
        result = _rewrite_tool_call_ids(messages)
        assert result == messages

    def test_mixed_standard_and_nonstandard(self):
        """Standard ids unchanged, non-standard rewritten, in same batch."""
        messages = [
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "call_good123",
                        "type": "function",
                        "function": {"name": "a", "arguments": "{}"},
                    },
                    {
                        "id": "bad:id:2",
                        "type": "function",
                        "function": {"name": "b", "arguments": "{}"},
                    },
                ],
            },
            {"role": "tool", "tool_call_id": "call_good123", "content": "r1"},
            {"role": "tool", "tool_call_id": "bad:id:2", "content": "r2"},
        ]
        result = _rewrite_tool_call_ids(messages)
        assert result[0]["tool_calls"][0]["id"] == "call_good123"
        assert result[1]["tool_call_id"] == "call_good123"
        bad_new = result[0]["tool_calls"][1]["id"]
        assert _CALL_PATTERN.match(bad_new)
        assert result[2]["tool_call_id"] == bad_new

    def test_original_messages_not_mutated(self):
        """Input messages must not be mutated in place."""
        tc = {"id": "bad:1", "type": "function", "function": {"name": "fn", "arguments": "{}"}}
        messages = [
            {"role": "assistant", "content": "", "tool_calls": [tc]},
            {"role": "tool", "tool_call_id": "bad:1", "content": "r"},
        ]
        _rewrite_tool_call_ids(messages)
        assert tc["id"] == "bad:1"
        assert messages[1]["tool_call_id"] == "bad:1"
