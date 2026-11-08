#!/bin/bash
# usage: run.sh <backend> <label>; runs the spike, records monitor/window placement mid-run
B=$1; L=$2
mkdir -p runs
( sleep 6; hyprctl clients -j | jq -c '.[] | select(.class=="spike" or .title=="spike") | {monitor, size}' > runs/$L.placement; hyprctl monitors -j | jq -c '[.[]|{id,refreshRate}]' >> runs/$L.placement ) &
SLINT_DEBUG_PERFORMANCE=${PERF:-} SLINT_BACKEND=$B ./target/release/spike > runs/$L.log 2>&1
wait
echo "== $L ($B)"; grep -v "^$" runs/$L.log | grep -E "rss|frames=" ; cat runs/$L.placement
