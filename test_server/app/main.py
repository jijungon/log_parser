import gzip
import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import httpx
import uvicorn
from fastapi import FastAPI, Header, HTTPException, Request

app = FastAPI(title="log_parser test server")

STORAGE_DIR = Path("/app/storage")
DOCS_DIR = Path("/app/docs")

# 경로별 저장 파일
STORAGE = {
    "ingest": STORAGE_DIR / "ingest.jsonl",    # log_parser push (/ingest)
    "stat":   STORAGE_DIR / "stat.jsonl",      # /pull/stat 응답
    "sos":    STORAGE_DIR / "sos.jsonl",       # /pull/sos 응답
}

RECV_TOKEN = os.environ.get("RECV_TOKEN", "test-token")
AGENT_HOST = os.environ.get("AGENT_HOST", "host.docker.internal")
STAT_TOKEN = os.environ.get("STAT_TOKEN", "")
SOS_TOKEN  = os.environ.get("SOS_TOKEN", "")


# ── 수신 ──────────────────────────────────────────────────────────────────────

@app.post("/ingest")
async def ingest(request: Request, authorization: Optional[str] = Header(None)):
    """log_parser push 수신 → storage/ingest.jsonl"""
    if authorization != f"Bearer {RECV_TOKEN}":
        raise HTTPException(status_code=401, detail="Unauthorized")

    raw = await request.body()
    if request.headers.get("content-encoding") == "gzip":
        raw = gzip.decompress(raw)

    try:
        envelope = json.loads(raw)
    except Exception:
        raise HTTPException(status_code=400, detail="Invalid JSON")

    seq = _save("ingest", envelope)
    _print_summary("ingest", envelope, seq)
    return {"ok": True, "seq": seq}


@app.get("/health")
async def health():
    return {"ok": True}


# ── pull ──────────────────────────────────────────────────────────────────────


@app.get("/pull/stat")
async def pull_stat():
    """stat_report /stat 호출 → storage/stat.jsonl"""
    async with httpx.AsyncClient() as client:
        r = await client.get(
            f"http://{AGENT_HOST}:9100/stat",
            headers={"Authorization": f"Bearer {STAT_TOKEN}"},
            timeout=10,
        )
    r.raise_for_status()
    envelope = r.json()  # httpx가 gzip 자동 해제
    seq = _save("stat", envelope)
    _print_summary("stat", envelope, seq)
    return {"ok": True, "seq": seq, "event_kind": envelope.get("event_kind")}


@app.post("/pull/sos")
async def pull_sos():
    """sos_report /trigger-sos 호출 → storage/sos.jsonl (blocking)"""
    async with httpx.AsyncClient() as client:
        r = await client.post(
            f"http://{AGENT_HOST}:9100/trigger-sos",
            headers={"Authorization": f"Bearer {SOS_TOKEN}"},
            timeout=120,
        )
    r.raise_for_status()
    envelope = r.json()  # httpx가 gzip 자동 해제
    seq = _save("sos", envelope)
    _print_summary("sos", envelope, seq)
    return {"ok": True, "seq": seq, "event_kind": envelope.get("event_kind")}


# ── 조회 ──────────────────────────────────────────────────────────────────────

@app.get("/envelopes")
async def list_envelopes(source: Optional[str] = None):
    """
    수신 목록. ?source=ingest|flush|stat|sos 로 필터 가능.
    생략 시 전체.
    """
    sources = [source] if source else list(STORAGE.keys())
    result = {}
    for src in sources:
        records = _load(src)
        result[src] = [_summarize(i + 1, r) for i, r in enumerate(records)]
    return result


@app.get("/envelopes/{source}/{seq}")
async def get_envelope(source: str, seq: int):
    """특정 envelope 상세. source: ingest|flush|stat|sos"""
    if source not in STORAGE:
        raise HTTPException(status_code=400, detail=f"unknown source: {source}")
    records = _load(source)
    if seq < 1 or seq > len(records):
        raise HTTPException(status_code=404, detail=f"{source} seq {seq} not found")
    return records[seq - 1]["envelope"]


@app.get("/logs")
async def get_logs(
    severity: Optional[str] = None,
    category: Optional[str] = None,
    source: Optional[str] = None,
    host: Optional[str] = None,
    fingerprint: Optional[str] = None,
    limit: int = 100,
):
    """
    수신된 모든 envelope에서 로그 이벤트를 추출해 한 번에 반환.

    Query params:
      ?severity=critical|error|warn|info
      ?category=system.general|auth.login|...
      ?source=journald|file.syslog|file.auth
      ?host=hostname  (서버별 필터)
      ?fingerprint=abc123  (특정 패턴만)
      ?limit=100  (기본값)
    """
    records = _load("ingest")
    events = []
    for rec in records:
        env = rec.get("envelope", {})
        received_at = rec.get("received_at", "")
        ev_host = env.get("cycle", {}).get("host", "")
        if host and ev_host != host:
            continue
        for section in env.get("body", []):
            for ev in section.get("data", []):
                if severity and ev.get("severity") != severity:
                    continue
                if category and ev.get("category") != category:
                    continue
                if source and ev.get("source") != source:
                    continue
                if fingerprint and ev.get("fingerprint") != fingerprint:
                    continue
                events.append({
                    "received_at": received_at,
                    "host": ev_host,
                    "severity": ev.get("severity"),
                    "category": ev.get("category"),
                    "source": ev.get("source"),
                    "fingerprint": ev.get("fingerprint"),
                    "count": ev.get("count"),
                    "template": ev.get("template"),
                    "sample_raws": ev.get("sample_raws", []),
                    "fields": ev.get("fields", {}),
                    "ts_first": ev.get("ts_first"),
                    "ts_last": ev.get("ts_last"),
                })

    events.sort(key=lambda x: x["ts_first"] or "", reverse=True)
    total = len(events)
    return {"total": total, "limit": limit, "events": events[:limit]}


@app.get("/hosts")
async def list_hosts():
    """
    보고 중인 모든 서버 목록 + 서버별 요약.

    각 서버의 마지막 수신 시각, 총 cycle 수, severity 합계를 반환.
    """
    records = _load("ingest")
    hosts: dict = {}
    for rec in records:
        env = rec.get("envelope", {})
        cycle = env.get("cycle", {})
        h = cycle.get("host", "unknown")
        received_at = rec.get("received_at", "")
        counts = env.get("headers", {}).get("counts", {}) or {}
        by_sev = counts.get("by_severity", {})

        if h not in hosts:
            hosts[h] = {
                "host": h,
                "host_id": cycle.get("host_id"),
                "boot_id": cycle.get("boot_id"),
                "first_seen": received_at,
                "last_seen": received_at,
                "envelope_count": 0,
                "total_critical": 0,
                "total_error": 0,
                "total_warn": 0,
                "total_info": 0,
                "last_seq": None,
            }
        s = hosts[h]
        s["envelope_count"] += 1
        s["last_seen"] = max(s["last_seen"], received_at)
        s["first_seen"] = min(s["first_seen"], received_at)
        s["total_critical"] += by_sev.get("critical", 0) or 0
        s["total_error"]    += by_sev.get("error", 0) or 0
        s["total_warn"]     += by_sev.get("warn", 0) or 0
        s["total_info"]     += by_sev.get("info", 0) or 0
        seq = env.get("cycle", {}).get("seq")
        if seq is not None:
            s["last_seq"] = seq

    return {"host_count": len(hosts), "hosts": sorted(hosts.values(), key=lambda x: x["last_seen"], reverse=True)}


@app.get("/compare")
async def compare_hosts(fingerprint: str):
    """
    특정 fingerprint 패턴이 어느 서버에서 몇 번 발생했는지 서버별 비교.

    ?fingerprint=abc123ef  (16자리 hex)

    활용: 동일 오류가 여러 서버에 퍼졌는지, 특정 서버만 집중됐는지 파악.
    """
    records = _load("ingest")
    by_host: dict = {}
    template_found = None

    for rec in records:
        env = rec.get("envelope", {})
        h = env.get("cycle", {}).get("host", "unknown")
        received_at = rec.get("received_at", "")
        for section in env.get("body", []):
            for ev in section.get("data", []):
                if ev.get("fingerprint") != fingerprint:
                    continue
                if template_found is None:
                    template_found = ev.get("template")
                if h not in by_host:
                    by_host[h] = {
                        "host": h,
                        "count": 0,
                        "cycles_seen": 0,
                        "ts_first": ev.get("ts_first"),
                        "ts_last": ev.get("ts_last"),
                        "sample_raws": [],
                        "severity": ev.get("severity"),
                        "category": ev.get("category"),
                    }
                e = by_host[h]
                e["count"] += ev.get("count", 1)
                e["cycles_seen"] += 1
                if ev.get("ts_first") and (e["ts_first"] is None or ev["ts_first"] < e["ts_first"]):
                    e["ts_first"] = ev["ts_first"]
                if ev.get("ts_last") and (e["ts_last"] is None or ev["ts_last"] > e["ts_last"]):
                    e["ts_last"] = ev["ts_last"]
                if len(e["sample_raws"]) < 3:
                    e["sample_raws"].extend(ev.get("sample_raws", []))

    if not by_host:
        raise HTTPException(status_code=404, detail=f"fingerprint {fingerprint} not found")

    hosts_sorted = sorted(by_host.values(), key=lambda x: x["count"], reverse=True)
    return {
        "fingerprint": fingerprint,
        "template": template_found,
        "affected_hosts": len(by_host),
        "total_count": sum(h["count"] for h in hosts_sorted),
        "by_host": hosts_sorted,
    }


@app.get("/analyze")
async def analyze():
    """docs/*.example 파일 구조와 수신 데이터 비교"""
    result = {}
    examples = sorted(DOCS_DIR.glob("*.example")) if DOCS_DIR.exists() else []

    for ex in examples:
        parsed = _extract_json(ex.read_text())
        if not parsed:
            continue
        kind = parsed.get("event_kind")
        src = _kind_to_source(kind)
        received = _load(src) if src else []
        result[ex.name] = {
            "event_kind": kind,
            "source_file": f"storage/{src}.jsonl" if src else None,
            "example_sections": len(parsed.get("body", [])),
            "example_section_names": [s.get("section") for s in parsed.get("body", [])],
            "received_count": len(received),
            "latest_received_at": received[-1]["received_at"] if received else None,
        }

    return result


# ── 내부 함수 ─────────────────────────────────────────────────────────────────

def _save(source: str, data: dict) -> int:
    path = STORAGE[source]
    path.parent.mkdir(parents=True, exist_ok=True)
    entry = {
        "received_at": datetime.now(timezone.utc).isoformat(),
        "envelope": data,
    }
    with path.open("a") as f:
        f.write(json.dumps(entry, ensure_ascii=False) + "\n")
    with path.open() as f:
        return sum(1 for _ in f)


def _load(source: str) -> list:
    path = STORAGE.get(source)
    if not path or not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def _summarize(seq: int, record: dict) -> dict:
    e = record.get("envelope", {})
    h = e.get("headers", {})
    bs = h.get("counts", {}).get("by_severity", {})
    return {
        "seq": seq,
        "received_at": record.get("received_at"),
        "event_kind": e.get("event_kind"),
        "host": e.get("cycle", {}).get("host"),
        "cycle_seq": e.get("cycle", {}).get("seq"),
        "critical": bs.get("critical"),
        "error": bs.get("error"),
        "warn": bs.get("warn"),
        "total_sections": h.get("total_sections"),
        "duration_ms": h.get("duration_ms"),
    }


def _print_summary(source: str, envelope: dict, seq: int):
    kind = envelope.get("event_kind", "unknown")
    host = envelope.get("cycle", {}).get("host", "?")
    h = envelope.get("headers", {})

    if kind == "log_batch":
        c = h.get("counts", {})
        print(f"[{_ts()}] ▶ log_batch  host={host}  cycle_seq={h.get('cycle_seq','?')}  {source}:{seq}")
        print(
            f"           critical={c.get('critical',0)}  error={c.get('error',0)}"
            f"  warn={c.get('warn',0)}  info={c.get('info',0)}"
        )
    else:
        print(
            f"[{_ts()}] ▶ {kind}  host={host}"
            f"  sections={h.get('total_sections','?')}  duration={h.get('duration_ms','?')}ms"
            f"  {source}:{seq}"
        )


def _ts() -> str:
    return datetime.now().strftime("%H:%M:%S")


def _extract_json(text: str) -> Optional[dict]:
    start = text.find("{")
    end = text.rfind("}")
    if start == -1 or end == -1:
        return None
    try:
        return json.loads(text[start:end + 1])
    except Exception:
        return None


def _kind_to_source(kind: Optional[str]) -> Optional[str]:
    return {"log_batch": "ingest", "stat_snapshot": "stat", "sos_snapshot": "sos"}.get(kind or "")


if __name__ == "__main__":
    port = int(os.environ.get("RECV_PORT", 8080))
    uvicorn.run(app, host="0.0.0.0", port=port, log_level="info")
