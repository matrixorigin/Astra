#!/usr/bin/env python3
"""Verify two existing Astra accounts; the caller must select non-admin test users."""

import json
import os
import secrets
import sys
import urllib.error
import urllib.request
from urllib.parse import quote


class VerificationError(Exception):
    pass


class HttpFailure(VerificationError):
    def __init__(self, message, status):
        super().__init__(message)
        self.status = status


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class Client:
    def __init__(self, base_url):
        # Match the parent shell's curl --noproxy '*'; never forward credentials
        # to a redirect target or an environment-configured proxy.
        self.base_url = base_url.rstrip('/')
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def request(self, stage, path, body=None, token=None):
        headers = {'Content-Type': 'application/json'}
        if token:
            headers['Authorization'] = 'Bearer ' + token
        request = urllib.request.Request(
            self.base_url + path,
            data=json.dumps(body).encode() if body is not None else None,
            headers=headers,
        )
        try:
            with self.opener.open(request, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            hint = ''
            if path.startswith('/memory/') and error.code == 403:
                # Match only the typed error code; never print an arbitrary
                # backend response or infer a cause from human-readable detail.
                try:
                    response = json.loads(error.read(8192))
                    code = response.get('error_code') if isinstance(response, dict) else None
                    if not isinstance(code, str):
                        code = None
                except (OSError, ValueError):
                    code = None
                hint = {
                    'memory_self_hosted_access_disabled': ' Check MEMORIA_SELF_HOSTED_MASTER_ACCESS=1 and compatible owner-scoped Memoria authentication.',
                    'memory_consent_denied': ' Review memory-sharing permissions; deployment settings do not override consent.',
                    'memory_access_disabled': ' Check the account connection and memory-sharing permissions.',
                }.get(code, ' Memory permission denied; check server diagnostics.')
            elif path.startswith('/memory/') and error.code == 401:
                hint = ' Check matching Memoria master keys and Memoria-Owner support (Memoria 0.5.2).'
            raise HttpFailure(f'{stage}: HTTP {error.code}.{hint}', error.code) from None
        except (OSError, ValueError, urllib.error.URLError):
            # Do not expose response bodies, passwords, tokens, or URL credentials.
            raise VerificationError(f'{stage}: transport failure or invalid JSON response') from None


def login(client, username, password):
    response = client.request('Password login', '/auth/login', {'username': username, 'password': password})
    token = response.get('access_token') if isinstance(response, dict) else None
    if not isinstance(token, str) or not token:
        raise VerificationError('Password login: no access token')
    user = client.request('Account identity', '/auth/me', token=token)
    if not isinstance(user, dict) or not isinstance(user.get('user_id'), str) or not user['user_id']:
        raise VerificationError('Account identity: missing user ID')
    # AuthUserResponse has no roles/is_admin fields. Do not claim that the
    # account role was verified here: the operator must choose non-admin users.
    return token, user['user_id']


def verify(client, credentials):
    owner_token, owner_id = login(client, *credentials[0])
    other_token, other_id = login(client, *credentials[1])
    if owner_id == other_id:
        raise VerificationError('Isolation check requires two different accounts')
    content = 'Astra violet cedar user memory verification ' + secrets.token_hex(16)
    memory_id = None
    primary_failure = False
    try:
        stored = client.request('User memory store', '/memory/store', {
            'content': content, 'memory_type': 'semantic',
        }, owner_token)
        if not isinstance(stored, dict) or not isinstance(stored.get('memory_id'), str) or not stored['memory_id']:
            raise VerificationError('Store returned no memory ID; a test record may need manual cleanup')
        memory_id = stored['memory_id']
        query = {'query': content, 'top_k': 100}
        own = client.request('User memory search', '/memory/search', query, owner_token)
        if not isinstance(own, list) or not any(
            isinstance(row, dict) and row.get('memory_id') == memory_id and row.get('content') == content
            for row in own
        ):
            raise VerificationError('User memory search did not return the exact stored ID and content')
        other = client.request('Other-user memory search', '/memory/search', query, other_token)
        if not isinstance(other, list):
            raise VerificationError('Other-user memory search returned an invalid payload')
        if any(isinstance(row, dict) and (row.get('memory_id') == memory_id or row.get('content') == content)
               for row in other):
            raise VerificationError('Memory isolation failed: another account can see the test memory')
        # Verify this exact ID is accessible to its owner before checking the
        # other account: a universally missing/broken route is not isolation.
        path = '/memory/expand/' + quote(memory_id, safe='')
        expanded = client.request('Owner memory read by ID', path, token=owner_token)
        if not isinstance(expanded, dict) or (
            expanded.get('memory_id') != memory_id or expanded.get('content') != content
        ):
            raise VerificationError('Owner memory read by ID did not return the exact stored record')
        try:
            other_expanded = client.request('Other-user memory read by ID', path, token=other_token)
        except HttpFailure as error:
            if error.status not in (403, 404):
                raise
        else:
            # Memoria's get_memory returns Option<MemoryResponse>: a missing
            # or foreign-owned ID is 200 + JSON null, not necessarily 404.
            if other_expanded is not None:
                raise VerificationError('Memory isolation failed: unexpected other-account read by ID response')
    except BaseException:
        primary_failure = True
        raise
    finally:
        if memory_id:
            try:
                purged = client.request('Test memory cleanup', '/memory/purge', {
                    'memory_ids': [memory_id], 'reason': 'Astra user memory verification cleanup',
                }, owner_token)
                if not isinstance(purged, dict) or (
                    purged.get('status') != 'completed'
                    or purged.get('requested_count') != 1
                    or purged.get('deleted_count') != 1
                    or purged.get('unresolved_count') != 0
                    or purged.get('requested_memory_ids') != [memory_id]
                ):
                    raise VerificationError('Test memory cleanup did not confirm one deleted record')
            except VerificationError as error:
                if primary_failure:
                    # Report cleanup independently without hiding the primary failure.
                    print(f'WARNING: test memory cleanup failed: {error}; inspect the test account.', file=sys.stderr)
                else:
                    raise


def main():
    names = ['ASTRA_SMOKE_USERNAME', 'ASTRA_SMOKE_PASSWORD',
             'ASTRA_SMOKE_OTHER_USERNAME', 'ASTRA_SMOKE_OTHER_PASSWORD']
    values = [os.environ.get(name, '') for name in names]
    if not all(values):
        raise VerificationError('Set all four ASTRA_SMOKE_USERNAME/PASSWORD and ASTRA_SMOKE_OTHER_USERNAME/PASSWORD variables')
    if len(sys.argv) != 2:
        raise VerificationError('Usage: verify_user_memory.py ASTRA_API_URL')
    print('Account roles are NOT verified by /auth/me; the operator must select two non-admin test accounts.', flush=True)
    verify(Client(sys.argv[1]), [(values[0], values[1]), (values[2], values[3])])
    print('✅ Astra account memory: password login, write, exact search, cross-account search/read-by-ID isolation, and cleanup passed')


if __name__ == '__main__':
    try:
        main()
    except VerificationError as error:
        print(f'Astra user memory verification failed: {error}', file=sys.stderr)
        sys.exit(1)
