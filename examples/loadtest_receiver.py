#!/usr/bin/env python3
"""loadtest 전송 측정용 최소 수신기 — 표준 라이브러리만 사용(설치 불필요).

파서/loadtest가 POST /ingest 로 보내는 gzip JSON envelope을 받아 바이트 수만 세고 200 반환.
실제 수신측(receiver_example.py)의 계약을 흉내낸 것으로, 전송 경로 속도 측정 전용이다.

실행:  python3 examples/loadtest_receiver.py [PORT]   (기본 8080)
그리고 loadtest를:  loadtest --gb 10 --endpoint http://127.0.0.1:8080/ingest
"""
import gzip
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

_total = {"reqs": 0, "wire_bytes": 0, "json_bytes": 0}


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(n) if n else b""
        body = raw
        if self.headers.get("Content-Encoding") == "gzip":
            try:
                body = gzip.decompress(raw)
            except OSError:
                body = raw
        _total["reqs"] += 1
        _total["wire_bytes"] += len(raw)
        _total["json_bytes"] += len(body)
        print(
            f"[recv] #{_total['reqs']} path={self.path} "
            f"wire={len(raw)}B json={len(body)}B "
            f"(누적 wire={_total['wire_bytes']}B)",
            flush=True,
        )
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *_a):  # 기본 접근 로그 억제
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    print(f"[recv] listening on 0.0.0.0:{port}  (POST /ingest)", flush=True)
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
