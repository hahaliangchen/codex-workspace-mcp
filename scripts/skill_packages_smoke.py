"""Exercise local package loading, persistence and MCP over real HTTP; no model calls."""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


class Harness:
    def __init__(self, directory):
        self.directory = directory
        self.agent_port, self.mcp_port = free_port(), free_port()
        env = dict(os.environ, DSH_HOME=str(directory / 'dsh'), DSH_PROFILE='test',
                   WORKSPACE_ROOT=str(directory / 'workspace'),
                   AGENT_BIND=f'127.0.0.1:{self.agent_port}', MCP_BIND=f'127.0.0.1:{self.mcp_port}')
        env.pop('DSH_SKILL_PACKAGES_DIR', None)
        (directory / 'workspace').mkdir(exist_ok=True)
        self.log = open(directory / 'server.log', 'ab')
        self.process = subprocess.Popen([str(ROOT / 'target/debug/codex-workspace-mcp')], env=env, cwd=ROOT,
                                        stdout=self.log, stderr=self.log, start_new_session=True)
        for _ in range(100):
            try:
                self.request('/agent/skill-packages')
                return
            except (OSError, urllib.error.HTTPError):
                if self.process.poll() is not None:
                    break
                time.sleep(.05)
        self.close()
        raise AssertionError((directory / 'server.log').read_text())

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.log.close()

    def request(self, path, body=None, method=None, token=None, mcp=False):
        headers = {'Content-Type': 'application/json'}
        if token:
            headers['Mcp-Session-Id'] = token
        request = urllib.request.Request(f'http://127.0.0.1:{self.mcp_port if mcp else self.agent_port}{path}',
                                         data=None if body is None else json.dumps(body).encode(), headers=headers, method=method)
        with urllib.request.urlopen(request, timeout=5) as response:
            raw = response.read()
            return response.headers, json.loads(raw) if raw else None

    def rpc(self, method, params=None, token=None):
        return self.request('/mcp', {'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params or {}}, token=token, mcp=True)


def main():
    with tempfile.TemporaryDirectory(prefix='skill-packages-smoke-') as directory:
        directory = Path(directory)
        package = directory / 'package'
        (package / 'page').mkdir(parents=True)
        (package / 'SKILL.md').write_text('---\nname: smoke\ndescription: Smoke test skill\n---\nOriginal instructions.\n')
        (package / 'backend.py').write_text("import json,sys\nr=json.load(sys.stdin)\nprint(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':r['params']}))\n")
        (package / 'page/index.html').write_text('<script src="/agent/skill-page-sdk.js"></script><p>Package page</p>')
        (package / 'plugin.json').write_text(json.dumps({
            'version': 1, 'id': 'smoke', 'title': 'Smoke', 'skill': 'SKILL.md', 'tools': ['smoke_echo'],
            'command_tools': [{'name': 'smoke_echo', 'description': 'Echo JSON arguments', 'input_schema': {'type': 'object'},
                               'program': 'python3', 'args': ['backend.py']}],
            'pages': [{'id': 'view', 'title': 'Smoke page', 'entry': 'page/index.html', 'renderer': 'html'}]}))
        server = Harness(directory)
        try:
            headers, _ = server.rpc('initialize')
            token = headers['Mcp-Session-Id']
            server.request('/agent/skill-packages', {'path': str(package)})
            names = lambda: {t['name'] for t in server.rpc('tools/list', token=token)[1]['result']['tools']}
            assert 'smoke_echo' not in names()
            stream = urllib.request.urlopen(urllib.request.Request(
                f'http://127.0.0.1:{server.mcp_port}/mcp', headers={'Mcp-Session-Id': token, 'Accept': 'text/event-stream'}), timeout=5)
            assert stream.headers['Content-Type'].startswith('text/event-stream')
            server.rpc('tools/call', {'name': 'read_skill', 'arguments': {'name': 'smoke'}}, token)
            lines = []
            while True:
                line = stream.readline().decode().strip()
                lines.append(line)
                if line.startswith('data:'):
                    assert 'notifications/tools/list_changed' in line
                    break
            stream.close()
            assert 'smoke_echo' in names()
            result = server.rpc('tools/call', {'name': 'smoke_echo', 'arguments': {'message': 'literal $(echo x)'}}, token)[1]
            assert result['result']['structuredContent']['arguments']['message'] == 'literal $(echo x)'
            with urllib.request.urlopen(f'http://127.0.0.1:{server.agent_port}/agent/skill-packages/smoke/pages/view/index') as response:
                assert b'Package page' in response.read()
            try:
                server.request('/agent/skill-packages/smoke/tools/read_code_flow', {})
                raise AssertionError('cross-package tool call accepted')
            except urllib.error.HTTPError as error:
                assert error.code == 400
            server.request('/agent/skill-packages/smoke/reload', {})
            assert 'smoke_echo' not in names()
            server.request('/agent/skill-packages/smoke/enabled', {'enabled': False})
            try:
                urllib.request.urlopen(f'http://127.0.0.1:{server.agent_port}/agent/skill-packages/smoke/pages/view/index')
                raise AssertionError('disabled page served')
            except urllib.error.HTTPError as error:
                assert error.code == 404
        finally:
            server.close()
        server = Harness(directory)
        try:
            packages = server.request('/agent/skill-packages')[1]['packages']
            assert next(p for p in packages if p['id'] == 'smoke')['enabled'] is False
            server.request('/agent/skill-packages/smoke', method='DELETE')
            assert package.exists(), 'removing a package must preserve its files'
        finally:
            server.close()
        server = Harness(directory)
        try:
            assert all(p['id'] != 'smoke' for p in server.request('/agent/skill-packages')[1]['packages'])
            server.request('/agent/skill-packages', {'path': str(package)})
            assert next(p for p in server.request('/agent/skill-packages')[1]['packages'] if p['id'] == 'smoke')['enabled']
            assert next(p for p in server.request('/agent/skill-packages')[1]['packages'] if p['id'] == 'code-flow')['origin'] == 'builtin'
            assert next(p for p in server.request('/agent/skill-packages')[1]['packages'] if p['id'] == 'smoke')['origin'] == 'external'
            server.request('/agent/skill-packages/code-flow/enabled', {'enabled': False})
        finally:
            server.close()
        server = Harness(directory)
        try:
            packages = server.request('/agent/skill-packages')[1]['packages']
            assert sum(p['id'] == 'code-flow' for p in packages) == 1
            assert next(p for p in packages if p['id'] == 'code-flow')['enabled'] is False
            server.request('/agent/skill-packages/code-flow/enabled', {'enabled': True})
        finally:
            server.close()
        print('PASS: dynamic install/RPC/SSE/reload/ownership/disable/restart/remove/reinstall')


if __name__ == '__main__':
    main()
