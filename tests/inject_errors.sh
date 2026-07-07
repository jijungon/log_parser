#!/usr/bin/env bash
# ⚠ 자동 생성됨 — 직접 수정 금지. 원본: error_cases.yaml → `python3 e2e_verify.py gen`
# VM(로그 수집 서버)에서 실행. logger 로 syslog/auth.log 에 합성 에러 로그를 써넣는다.
#   ./inject_errors.sh            # 전체 케이스 주입
#   ./inject_errors.sh kernel.oom # 특정 카테고리만
set -euo pipefail
target="${1:-all}"
echo "[inject] target=$target"

if [[ "$target" == "all" || "$target" == "kernel.oom" ]]; then
  logger -t kernel -p kern.err "Out of memory: Killed process 2481 (java)"
  echo "  → [kernel.oom] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "auth.bruteforce" ]]; then
  logger -t sshd -p authpriv.info "Failed password for invalid user admin from 203.0.113.7 port 51022 ssh2"
  echo "  → [auth.bruteforce] sshd/authpriv.info"
fi
if [[ "$target" == "all" || "$target" == "auth.event" ]]; then
  logger -t sshd -p authpriv.info "Accepted password for root from 203.0.113.7 port 51044 ssh2"
  echo "  → [auth.event] sshd/authpriv.info"
fi
if [[ "$target" == "all" || "$target" == "disk.io_error" ]]; then
  logger -t kernel -p kern.err "blk_update_request: I/O error, dev sda, sector 1234567 op 0x0:(READ)"
  echo "  → [disk.io_error] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "systemd.unit_failure" ]]; then
  logger -t systemd -p daemon.err "Failed to start app-worker.service - Application Worker."
  echo "  → [systemd.unit_failure] systemd/daemon.err"
fi
if [[ "$target" == "all" || "$target" == "fs.error" ]]; then
  logger -t kernel -p kern.err "EXT4-fs error (device sda1): ext4_find_entry:1234: inode #2: comm ls: reading directory lblock 0"
  echo "  → [fs.error] kernel/kern.err"
fi

echo "[inject] 완료. 로컬에서 e2e_verify.py verify 실행하세요."
