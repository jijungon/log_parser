# log_parser 수신측(receiver) 참조 예시 — FastAPI
#
# 파서가 30분마다 POST /ingest 로 보내는 gzip JSON envelope을 받는 최소 구현 + 소비 예시.
# 이 파서 프로젝트의 일부가 아니라, "받는 쪽" 구현 참고용이다.
# envelope/타입 정의: docs/RECEIVER_TYPE_SPEC.md, 계약: docs/4_RECEIVER_CONTRACT.md

import gzip
import json
from typing import Optional

from fastapi import FastAPI, Header, HTTPException, Request

app = FastAPI()
RECV_TOKEN = "your-secret-token"  # 파서의 PUSH_OUTBOUND_TOKEN 과 일치해야 함


@app.post("/ingest")
async def ingest(request: Request, authorization: Optional[str] = Header(None)):
    # 1. 인증
    if authorization != f"Bearer {RECV_TOKEN}":
        raise HTTPException(status_code=401)

    # 2. gzip 해제
    raw = await request.body()
    if request.headers.get("content-encoding") == "gzip":
        raw = gzip.decompress(raw)

    # 3. 파싱
    envelope = json.loads(raw)
    kind = envelope["event_kind"]
    host = envelope["cycle"]["host"]

    # 4. 중복 수신 방지 (멱등) — 아래 is_duplicate 참조
    #    if is_duplicate(envelope, seen): return {"ok": True}

    # 5. log_batch 처리
    if kind == "log_batch":
        counts = envelope["headers"].get("counts", {}).get("by_severity", {})
        print(f"[{host}] critical={counts.get('critical', 0)} error={counts.get('error', 0)}")

        for section in envelope.get("body", []):
            for event in section.get("data", []):
                if event["severity"] in ("critical", "error"):
                    print(f"  [{event['severity']}] {event['category']} | {event['template']}")
                    print(f"    count={event['count']} fingerprint={event['fingerprint']}")

    # 6. 성공 응답 (2xx 반환 필수 — 파서가 spool에서 삭제)
    return {"ok": True}


# ── 중복 수신 방지 (멱등) ──────────────────────────────────────────────────────
# 파서는 네트워크 오류 시 재전송한다. 고유 키 (host_id, boot_id, seq)로 dedup.
# 주의: stat_snapshot/sos_snapshot 은 seq 필드가 없어 ts 보조 키 + upsert 권장.
def is_duplicate(envelope: dict, seen: set) -> bool:
    cycle = envelope["cycle"]
    key = (cycle["host_id"], cycle["boot_id"], cycle.get("seq"))
    if key in seen:
        return True
    seen.add(key)
    return False


# ── 필터링 예시 ────────────────────────────────────────────────────────────────
def filtering_examples(envelope: dict):
    body = envelope.get("body", [])

    # critical 이벤트만 추출
    critical_events = [
        e for section in body for e in section.get("data", [])
        if e["severity"] == "critical"
    ]

    # 특정 category 필터
    oom_events = [
        e for section in body for e in section.get("data", [])
        if e["category"] == "kernel.oom"
    ]

    # 반복 발생 이벤트 (count 기반)
    repeated = [
        e for section in body for e in section.get("data", [])
        if e["count"] >= 5
    ]

    # SSH 로그인 실패 유저 목록
    auth_failures = [
        (e["fields"].get("user"), e["count"])
        for section in body for e in section.get("data", [])
        if e["category"] == "auth.failure" and "user" in e.get("fields", {})
    ]

    return critical_events, oom_events, repeated, auth_failures
