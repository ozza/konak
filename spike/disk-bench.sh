#!/bin/bash
# Usage: disk-bench.sh <label> <target-dir>
# Writes ~20 GB of photo-like (3 MB) and video-like (1 GB) files, then times a metadata scan and a full read.
set -u
L=$1 T=$2
mkdir -p "$T" /root/seed
[ -f /root/seed/photo ] || head -c 3M /dev/urandom > /root/seed/photo
[ -f /root/seed/video ] || head -c 1G /dev/urandom > /root/seed/video
rm -rf "$T/bench"; mkdir -p "$T/bench/photos" "$T/bench/videos"; sync
t0=$(date +%s.%N)
for i in $(seq 1 2000); do cp /root/seed/photo "$T/bench/photos/p$i.jpg"; done
t1=$(date +%s.%N)
for i in $(seq 1 14); do cp /root/seed/video "$T/bench/videos/v$i.mp4"; done; sync
t2=$(date +%s.%N)
sync; echo 3 > /proc/sys/vm/drop_caches
t3=$(date +%s.%N); find "$T/bench" -type f -printf '%s\n' | wc -l >/dev/null; t4=$(date +%s.%N)
cat "$T/bench/videos/"* > /dev/null; t5=$(date +%s.%N)
awk -v l="$L" -v a=$t0 -v b=$t1 -v c=$t2 -v d=$t3 -v e=$t4 -v f=$t5 'BEGIN{
 printf "{\"layout\":\"%s\",\"small_write_MBps\":%.0f,\"large_write_MBps\":%.0f,\"scan_2014_files_s\":%.2f,\"large_read_MBps\":%.0f}\n",
 l, 6000/(b-a), 14336/(c-b), e-d, 14336/(f-e)}'
rm -rf "$T/bench"
