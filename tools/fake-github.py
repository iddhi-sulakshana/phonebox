"""A stand-in for GitHub's release API and downloads on 127.0.0.1, for the self-update round trip
(agent-rs/scripts/remote-update-check.sh). The agent reaches it through PHONEBOX_UPDATE_API.

    python3 tools/fake-github.py <folder> <port>

- GET /repos/<owner>/<repo>/releases?...      <folder>/releases.json, read on every request, so a check can switch
                                              releases between rounds
- GET /repos/<owner>/<repo>/releases/latest   the first release in releases.json that is neither a draft nor a
                                              pre-release (what a final agent asks for); 404 when there is none
- GET /download/<name>                        <folder>/<name> (an asset's browser_download_url)
- GET /repos/<o>/<r>/releases/assets/<name>   <folder>/<name> (an asset's API url, the fallback)

Each request is appended to <folder>/requests.log as "METHOD path authorization=yes|no": the GitHub token must never
reach a server other than api.github.com.
"""
import http.server
import json
import pathlib
import sys
import urllib.parse


def release(port, tag, names, prerelease=True, draft=False):
    """A release object as the API returns it, with its assets served from this server."""
    base = f'http://127.0.0.1:{port}'
    return {
        'tag_name': tag,
        'name': f'PhoneBox {tag.lstrip("v")}',
        'draft': draft,
        'prerelease': prerelease,
        'html_url': f'{base}/releases/tag/{tag}',
        'assets': [{
            'name': name,
            'browser_download_url': f'{base}/download/{name}',
            'url': f'{base}/repos/fake/phonebox/releases/assets/{name}',
        } for name in names],
    }


class Handler(http.server.BaseHTTPRequestHandler):
    folder = pathlib.Path('.')

    def log_message(self, *args):
        pass

    def send(self, code, body=b'', kind='application/json'):
        self.send_response(code)
        self.send_header('Content-Type', kind)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = urllib.parse.urlsplit(self.path).path
        with open(self.folder / 'requests.log', 'a', encoding='utf-8') as log:
            log.write(f'GET {self.path} authorization={"yes" if self.headers.get("Authorization") else "no"}\n')
        parts = path.strip('/').split('/')
        if len(parts) == 4 and parts[0] == 'repos' and parts[3] == 'releases':
            listed = self.folder / 'releases.json'
            return self.send(200, listed.read_bytes() if listed.exists() else b'[]')
        if len(parts) == 5 and parts[0] == 'repos' and parts[3:] == ['releases', 'latest']:
            listed = self.folder / 'releases.json'
            full = [r for r in (json.loads(listed.read_text()) if listed.exists() else [])
                    if not r.get('draft') and not r.get('prerelease')]
            return self.send(200, json.dumps(full[0]).encode()) if full else self.send(404, b'{"message": "Not Found"}')
        name = None
        if len(parts) == 2 and parts[0] == 'download':
            name = parts[1]
        elif len(parts) == 6 and parts[0] == 'repos' and parts[3:5] == ['releases', 'assets']:
            name = parts[5]
        if name and '/' not in name and not name.startswith('.'):
            asset = self.folder / name
            if asset.is_file():
                return self.send(200, asset.read_bytes(), 'application/octet-stream')
        return self.send(404, b'{"message": "Not Found"}')


def main(argv):
    if argv[:1] == ['release']:
        # python3 tools/fake-github.py release <port> <tag> <asset names...>  -> one release object as JSON (a
        # pre-release when the tag has a "-", as release.yml publishes them)
        print(json.dumps(release(int(argv[1]), argv[2], argv[3:], prerelease='-' in argv[2])))
        return
    Handler.folder = pathlib.Path(argv[0]).resolve()
    server = http.server.ThreadingHTTPServer(('127.0.0.1', int(argv[1])), Handler)
    server.serve_forever()


if __name__ == '__main__':
    main(sys.argv[1:])
