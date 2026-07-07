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
if [[ "$target" == "all" || "$target" == "kernel.bug" ]]; then
  logger -t kernel -p kern.err "kernel BUG at mm/slub.c:305!"
  echo "  → [kernel.bug] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "kernel.panic" ]]; then
  logger -t kernel -p kern.emerg "Kernel panic - not syncing: Fatal exception"
  echo "  → [kernel.panic] kernel/kern.emerg"
fi
if [[ "$target" == "all" || "$target" == "process.crash" ]]; then
  logger -t kernel -p kern.err "myapp[12345]: segfault at 7f3a ip 00007f3a sp 00007ffe error 4 in libc.so.6"
  echo "  → [process.crash] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "fs.error" ]]; then
  logger -t kernel -p kern.err "EXT4-fs error (device sda1): ext4_find_entry:1234: inode #2: comm ls: reading directory lblock 0"
  echo "  → [fs.error] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "fs.readonly" ]]; then
  logger -t kernel -p kern.err "EXT4-fs (sda1): Remounting filesystem read-only"
  echo "  → [fs.readonly] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "hw.mce" ]]; then
  logger -t kernel -p kern.err "mce: [Hardware Error]: Machine check events logged"
  echo "  → [hw.mce] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "disk.smart_error" ]]; then
  logger -t smartd -p daemon.err "Device: /dev/sda, SMART Prefailure Attribute: 5 Reallocated_Sector_Ct Threshold Exceeded"
  echo "  → [disk.smart_error] smartd/daemon.err"
fi
if [[ "$target" == "all" || "$target" == "disk.io_error" ]]; then
  logger -t kernel -p kern.err "blk_update_request: I/O error, dev sda, sector 1234567 op 0x0:(READ)"
  echo "  → [disk.io_error] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "disk.link_error" ]]; then
  logger -t kernel -p kern.err "ata3: hard resetting link"
  echo "  → [disk.link_error] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "net.error" ]]; then
  logger -t kernel -p kern.warning "nf_conntrack: table full, dropping packet"
  echo "  → [net.error] kernel/kern.warning"
fi
if [[ "$target" == "all" || "$target" == "net.watchdog" ]]; then
  logger -t kernel -p kern.err "NETDEV WATCHDOG: eth0 (e1000e): transmit queue 0 timed out"
  echo "  → [net.watchdog] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "systemd.unit_failure" ]]; then
  logger -t systemd -p daemon.err "Failed to start app-worker.service - Application Worker."
  echo "  → [systemd.unit_failure] systemd/daemon.err"
fi
if [[ "$target" == "all" || "$target" == "systemd.restart_loop" ]]; then
  logger -t systemd -p daemon.err "app-worker.service: Start request repeated too quickly."
  echo "  → [systemd.restart_loop] systemd/daemon.err"
fi
if [[ "$target" == "all" || "$target" == "auth.bruteforce" ]]; then
  logger -t sshd -p authpriv.info "Failed password for invalid user admin from 203.0.113.7 port 51022 ssh2"
  echo "  → [auth.bruteforce] sshd/authpriv.info"
fi
if [[ "$target" == "all" || "$target" == "auth.failure" ]]; then
  logger -t sshd -p authpriv.info "Failed password for root from 198.51.100.9 port 40122 ssh2"
  echo "  → [auth.failure] sshd/authpriv.info"
fi
if [[ "$target" == "all" || "$target" == "auth.event" ]]; then
  logger -t sshd -p authpriv.info "Accepted password for root from 203.0.113.7 port 51044 ssh2"
  echo "  → [auth.event] sshd/authpriv.info"
fi
if [[ "$target" == "all" || "$target" == "session.activity" ]]; then
  logger -t systemd-logind -p auth.info "New session 4521 of user deploy."
  echo "  → [session.activity] systemd-logind/auth.info"
fi
if [[ "$target" == "all" || "$target" == "ntp.drift" ]]; then
  logger -t chronyd -p daemon.info "System clock wrong by 4.521 seconds, adjusting"
  echo "  → [ntp.drift] chronyd/daemon.info"
fi
if [[ "$target" == "all" || "$target" == "container.oom" ]]; then
  logger -t kernel -p kern.err "Memory cgroup out of memory: Task in /docker/9a3b killed as a result of limit of /docker/9a3b"
  echo "  → [container.oom] kernel/kern.err"
fi
if [[ "$target" == "all" || "$target" == "selinux.denial" ]]; then
  logger -t kernel -p kern.warning "type=1400 audit(1720000000.123:456): avc: denied { open } for pid=999 comm=\"httpd\" name=\"secret\""
  echo "  → [selinux.denial] kernel/kern.warning"
fi

echo "[inject] 완료. 로컬에서 e2e_verify.py verify 실행하세요."
