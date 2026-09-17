#!/usr/bin/env python3
"""Offline regressions for the ordinary-user memory smoke test."""

import contextlib
import importlib.util
import io
from pathlib import Path
import unittest
import urllib.error
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / 'ops' / 'verify_user_memory.py'
spec = importlib.util.spec_from_file_location('verify_user_memory', SCRIPT)
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)


class FakeClient:
    def __init__(self, *, denied=False, leaks=False, malformed=False, cleanup_fails=False,
                 id_leaks=False, id_denial=None, owner_id_missing=False, cleanup_receipt=None):
        self.denied, self.leaks, self.malformed = denied, leaks, malformed
        self.cleanup_fails = cleanup_fails
        self.id_leaks, self.id_denial = id_leaks, id_denial
        self.owner_id_missing = owner_id_missing
        self.cleanup_receipt = cleanup_receipt
        self.calls = []
        self.memory = None

    def request(self, stage, path, body=None, token=None):
        self.calls.append((path, body, token))
        if path == '/auth/login':
            return {'access_token': body['username']}
        if path == '/auth/me':
            # AuthUserResponse on the real /auth/me boundary (no role fields).
            return {'user_id': token, 'username': token,
                    'email': token + '@example.invalid', 'display_name': None}
        if self.denied:
            raise verify.VerificationError('User memory store: HTTP 403')
        if path == '/memory/store':
            self.memory = {'memory_id': 'test-id', 'content': body['content']}
            return self.memory
        if path == '/memory/search':
            if self.malformed:
                return {'unexpected': []}
            return [self.memory] if token == 'owner' or self.leaks else []
        if path.startswith('/memory/expand/'):
            if token == 'owner' and self.owner_id_missing:
                return None
            if token == 'owner' or self.id_leaks:
                return self.memory
            if self.id_denial is None:
                # Actual Memoria 0.5.2 get_memory: foreign ID is JSON null.
                return None
            raise verify.HttpFailure('Other-user ID read rejected', self.id_denial)
        if path == '/memory/purge':
            if self.cleanup_fails:
                raise verify.VerificationError('purge-backend-unavailable')
            # normalize_exact_memory_purge_receipt, not the upstream Memoria receipt.
            return self.cleanup_receipt if self.cleanup_receipt is not None else {
                'status': 'completed', 'requested_count': 1, 'deleted_count': 1,
                'unresolved_count': 0, 'requested_memory_ids': ['test-id'],
                'identity_resolution': 'all_requested_confirmed', 'receipt_source': 'memoria_purge',
                'message': 'memory_purge: backend confirmed all 1 exact entries were removed',
            }
        raise AssertionError(path)


class UserMemoryVerificationTests(unittest.TestCase):
    credentials = [('owner', 'hidden-one'), ('other', 'hidden-two')]

    def test_success_uses_user_boundary_and_cleans_exact_created_id(self):
        client = FakeClient()
        verify.verify(client, self.credentials)
        path, body, token = client.calls[-1]
        self.assertEqual((path, body['memory_ids'], token), ('/memory/purge', ['test-id'], 'owner'))
        self.assertEqual(len([c for c in client.calls if c[0] == '/auth/login']), 2)
        self.assertTrue(all(not c[0].startswith('/v1/') for c in client.calls))

    def test_disabled_access_fails_without_purging_unknown_records(self):
        client = FakeClient(denied=True)
        with self.assertRaisesRegex(verify.VerificationError, '403'):
            verify.verify(client, self.credentials)
        self.assertFalse(any(c[0] == '/memory/purge' for c in client.calls))

    def test_leaked_memory_fails_and_still_cleans_up(self):
        client = FakeClient(leaks=True)
        with self.assertRaisesRegex(verify.VerificationError, 'isolation failed'):
            verify.verify(client, self.credentials)
        self.assertEqual(client.calls[-1][0], '/memory/purge')

    def test_malformed_search_is_not_success(self):
        with self.assertRaisesRegex(verify.VerificationError, 'exact stored ID'):
            verify.verify(FakeClient(malformed=True), self.credentials)

    def test_cleanup_failure_does_not_mask_primary_error(self):
        with contextlib.redirect_stderr(io.StringIO()) as stderr:
            with self.assertRaisesRegex(verify.VerificationError, 'isolation failed'):
                verify.verify(FakeClient(leaks=True, cleanup_fails=True), self.credentials)
        self.assertIn('purge-backend-unavailable', stderr.getvalue())

    def test_cleanup_failure_alone_fails_verification(self):
        with self.assertRaisesRegex(verify.VerificationError, 'purge-backend-unavailable'):
            verify.verify(FakeClient(cleanup_fails=True), self.credentials)

    def test_rejects_same_account(self):
        with self.assertRaisesRegex(verify.VerificationError, 'different accounts'):
            verify.verify(FakeClient(), [self.credentials[0], self.credentials[0]])

    def test_http_errors_are_actionable_without_echoing_secrets(self):
        client = verify.Client('http://127.0.0.1:1')
        for status, body, expected in [
            (403, b'{"error_code":"memory_self_hosted_access_disabled","detail":"secret"}', 'MEMORIA_SELF_HOSTED_MASTER_ACCESS'),
            (403, b'{"error_code":"memory_consent_denied","detail":"secret"}', 'Review memory-sharing'),
            (403, b'secret', 'check server diagnostics'),
            (403, b'{"error_code":[],"detail":"secret"}', 'check server diagnostics'),
            (401, b'secret', 'Memoria-Owner'),
        ]:
            error = urllib.error.HTTPError('http://example.invalid', status, 'secret', {}, io.BytesIO(body))
            with patch.object(client.opener, 'open', side_effect=error):
                with self.assertRaises(verify.VerificationError) as raised:
                    client.request('memory', '/memory/search', {}, 'secret-token')
            self.assertIn(expected, str(raised.exception))
            self.assertNotIn('secret', str(raised.exception))

    def test_cleanup_failure_is_not_swallowed_inside_an_outer_except(self):
        try:
            raise ValueError('already handled')
        except ValueError:
            with self.assertRaisesRegex(verify.VerificationError, 'purge-backend-unavailable'):
                verify.verify(FakeClient(cleanup_fails=True), self.credentials)

    def test_rejects_legacy_or_unconfirmed_cleanup_receipts(self):
        for receipt in [{'purged': 1}, {'deleted_count': 1}, {'status': 'partial'}]:
            with self.assertRaisesRegex(verify.VerificationError, 'did not confirm'):
                verify.verify(FakeClient(cleanup_receipt=receipt), self.credentials)

    def test_id_read_must_be_denied_even_when_search_does_not_leak(self):
        client = FakeClient(id_leaks=True)
        with self.assertRaisesRegex(verify.VerificationError, 'by ID'):
            verify.verify(client, self.credentials)
        self.assertEqual(client.calls[-1][0], '/memory/purge')

    def test_only_403_or_404_are_valid_cross_account_id_denials(self):
        for status in [403, 404]:
            verify.verify(FakeClient(id_denial=status), self.credentials)
        for status in [401, 429, 500]:
            with self.assertRaises(verify.HttpFailure):
                verify.verify(FakeClient(id_denial=status), self.credentials)

    def test_universally_missing_id_is_not_isolation(self):
        client = FakeClient(owner_id_missing=True)
        with self.assertRaisesRegex(verify.VerificationError, 'Owner memory read by ID'):
            verify.verify(client, self.credentials)
        self.assertEqual(client.calls[-1][0], '/memory/purge')

    def test_redirects_cannot_forward_credentials(self):
        self.assertIsNone(verify.NoRedirect().redirect_request(None, None, 302, '', {}, 'https://other.invalid'))


if __name__ == '__main__':
    unittest.main()
