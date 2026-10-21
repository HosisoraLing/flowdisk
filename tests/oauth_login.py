#!/usr/bin/env python3
"""Direct OAuth setup checks; no real provider account or authorization exchange."""
import json
import stat
import sys
import tempfile
import threading
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import urlparse, parse_qs
from control import App, Gateway, Server, reject


def main():
    gateway = Server(('127.0.0.1', 0), Gateway)
    threading.Thread(target=gateway.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='flowdisk-oauth-') as tmp:
        root = Path(tmp)
        app = App(sys.argv[1], root, gateway)
        try:
            for provider, host in [('google', 'accounts.google.com'), ('onedrive', 'login.microsoftonline.com')]:
                path = '/accounts/' + provider
                reject(app, path + '/oauth-config', auth=False, code=401)
                session = app.api(path + '/login', {'mode': 'oauth'})
                assert session['setup_required'] and 'helper_url' not in session
                settings = {'client_id': 'fixture-' + provider, 'client_secret': 'fixture-secret-' + provider, 'public_url': 'https://download.example.com'}
                reject(app, path + '/oauth-config', settings, method='PUT', auth=False, code=401)
                reject(app, path + '/oauth-config', {**settings, 'public_url': 'http://download.example.com'}, method='PUT')
                if provider == 'google':
                    reject(app, path + '/oauth-config', {**settings, 'public_url': 'https://192.0.2.1'}, method='PUT')
                saved = app.api(path + '/oauth-config', settings, method='PUT')
                assert saved['callback_uri'] == 'https://download.example.com/oauth/' + provider + '/callback'
                view = app.api(path + '/oauth-config')
                assert view['configured'] and view['has_secret'] and 'client_secret' not in view
                assert settings['client_secret'] not in json.dumps(view)
                app.api(path + '/oauth-config', {**settings, 'client_secret': ''}, method='PUT')
                reject(app, path + '/oauth-config', {**settings, 'client_id': 'different-app', 'client_secret': ''}, method='PUT')
                session = app.api(path + '/login', {'mode': 'oauth'})
                url = urlparse(session['authorize_url'])
                query = parse_qs(url.query)
                assert url.scheme == 'https' and url.hostname == host
                assert query['client_id'] == [settings['client_id']]
                assert query['redirect_uri'] == [saved['callback_uri']]
                assert query['state'] == [session['id']] and query['code_challenge_method'] == ['S256']
                assert len(query['code_challenge'][0]) == 43
                assert settings['client_secret'] not in json.dumps(session) and 'code_verifier' not in json.dumps(session)
                other = 'onedrive' if provider == 'google' else 'google'
                for suffix in [other + '/callback?state=' + session['id'] + '&code=fixture', provider + '/callback?state=' + session['id'] + '&error=access_denied']:
                    try:
                        urllib.request.urlopen(app.base + '/oauth/' + suffix)
                    except urllib.error.HTTPError as error:
                        assert error.code == 400
                    else:
                        raise AssertionError('invalid or cancelled authorization accepted')
                assert app.api('/logins/' + session['id'])['status'] == 'cancelled'
            app.api('/accounts/onedrive/oauth-config', {'client_id':'public-device-app','client_secret':'','public_url':''}, method='PUT')
            assert app.api('/accounts/onedrive/oauth-config')['configured']
            try:
                urllib.request.urlopen(app.base + '/internal/onedrive/renew?key=wrong&client_id=public-device-app&refresh_ui=fixture-refresh')
            except urllib.error.HTTPError as error:
                assert error.code == 401
            else:
                raise AssertionError('internal token refresh accepted without its private key')
            config_file = root / 'data' / 'oauth-clients.json'
            assert stat.S_IMODE(config_file.stat().st_mode) == 0o600
            assert len(json.loads(config_file.read_text())) == 2
            app.stop()
            app.start()
            for provider in ['google', 'onedrive']:
                assert app.api('/accounts/' + provider + '/oauth-config')['configured']
                if provider == 'google':
                    assert 'authorize_url' in app.api('/accounts/' + provider + '/login', {'mode': 'oauth'})
                else:
                    assert app.api('/accounts/onedrive/oauth-config')['public_url'] == ''
            print('PASS direct official URLs, PKCE/state, local setup, secret redaction, permissions and restart persistence')
        finally:
            app.stop()
            app.log.close()
            gateway.shutdown()


if __name__ == '__main__':
    main()
