#!/bin/bash
# mon.sh DEV PROC_REGEX INTERVAL OUT
# Samples one block device (from /proc/diskstats) and the CPU of this user's processes whose
# command line matches PROC_REGEX. One CSV row per interval. No root needed.
dev=$1 re=$2 iv=${3:-2} out=$4
hz=$(getconf CLK_TCK)
read_disk() { awk -v d="$dev" '$3==d{print $4,$6,$8,$10,$13}' /proc/diskstats; }  # rd_ios rd_sect wr_ios wr_sect io_ms
read_cpu() {
  local t=0
  for p in $(pgrep -u "$(id -u)" -f "$re"); do
    [ "$p" = "$$" ] && continue
    s=$(awk '{print $14+$15}' /proc/$p/stat 2>/dev/null) && t=$((t + s))
  done
  echo $t
}
read_sys() { awk '/^cpu /{print $2+$3+$4+$5+$6+$7+$8, $6}' /proc/stat; }  # total, iowait
echo "t,util_pct,rMBs,wMBs,r_iops,w_iops,r_kb_per_io,w_kb_per_io,proc_cpu_pct,sys_iowait_pct" > "$out"
set -- $(read_disk); a=("$@"); c0=$(read_cpu); set -- $(read_sys); s0=("$@"); t0=$(date +%s.%N)
while sleep "$iv"; do
  set -- $(read_disk); b=("$@"); c1=$(read_cpu); set -- $(read_sys); s1=("$@"); t1=$(date +%s.%N)
  awk -v t0="$t0" -v t1="$t1" -v hz="$hz" \
      -v r0="${a[0]}" -v rs0="${a[1]}" -v w0="${a[2]}" -v ws0="${a[3]}" -v io0="${a[4]}" \
      -v r1="${b[0]}" -v rs1="${b[1]}" -v w1="${b[2]}" -v ws1="${b[3]}" -v io1="${b[4]}" \
      -v c0="$c0" -v c1="$c1" -v st0="${s0[0]}" -v sw0="${s0[1]}" -v st1="${s1[0]}" -v sw1="${s1[1]}" 'BEGIN{
    dt=t1-t0; ri=r1-r0; wi=w1-w0; rk=(rs1-rs0)/2; wk=(ws1-ws0)/2; cpu=c1-c0; if (cpu<0) cpu=0
    printf "%d,%.1f,%.1f,%.1f,%.0f,%.0f,%.0f,%.0f,%.1f,%.2f\n", t1, (io1-io0)/(dt*10), rk/1024/dt, wk/1024/dt,
      ri/dt, wi/dt, (ri?rk/ri:0), (wi?wk/wi:0), cpu/hz/dt*100, (st1>st0?(sw1-sw0)*100/(st1-st0):0) }' >> "$out"
  a=("${b[@]}"); c0=$c1; s0=("${s1[@]}"); t0=$t1
done
