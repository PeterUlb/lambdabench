# Minimal Lambda runtime API: holds the first /next until /tmp/go exists, so a
# profile can be dumped at the end of Init; marks each /next in /tmp/nextN.
# With EXIT_AFTER_FIRST=1 the second /next fails instead, which ends the function.
import http.server, os, socketserver, sys, time, threading
EVENT = open(sys.argv[1], 'rb').read()
count = {'next': 0}
lock = threading.Lock()
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        with lock:
            count['next'] += 1
            n = count['next']
        open(f'/tmp/next{n}', 'w').close()
        if n > 1:
            # EXIT_AFTER_FIRST=1: drop the connection, so the runtime's /next fails and the
            # function exits normally (callgrind writes its profile at exit).
            if os.environ.get('EXIT_AFTER_FIRST') == '1':
                self.close_connection = True
                return
            time.sleep(3600)
            return
        while not os.path.exists('/tmp/go'):
            time.sleep(0.05)
        self.send_response(200)
        self.send_header('Lambda-Runtime-Aws-Request-Id', 'req-1')
        self.send_header('Lambda-Runtime-Deadline-Ms', str(int(time.time() * 1000) + 600000))
        self.send_header('Lambda-Runtime-Invoked-Function-Arn', 'arn:aws:lambda:eu-central-1:000000000000:function:probe')
        self.send_header('Lambda-Runtime-Trace-Id', 'Root=1-00000000-000000000000000000000000;Parent=0000000000000000;Sampled=0')
        self.send_header('Content-Length', str(len(EVENT)))
        self.end_headers()
        self.wfile.write(EVENT)
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', 0) or 0))
        open('/tmp/posted', 'ab').write(self.path.encode() + b' ' + body[:300] + b'\n')
        self.send_response(202); self.send_header('Content-Length', '0'); self.end_headers()
class S(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
S(('127.0.0.1', 9001), H).serve_forever()
