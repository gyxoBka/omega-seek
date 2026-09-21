#!/bin/sh
# One line per repository: what an agent is shown (eight answers) for agent-style
# queries, and whether a symbol lookup puts the declaration first.
#
# Which repositories, and where their probes and checkouts are, is read from
# eval/repos.local -- one `name|probes-dir|checkout` per line, never committed,
# so probes written against private code stay private. See repos.local.example.
#
# Columns: agent span R@1, found within what is shown, MRR | symbol span R@1.
EXE="${OMEGA_BIN:-target/release/omega}"
CONF="${OMEGA_EVAL_REPOS:-eval/repos.local}"
[ -f "$CONF" ] || { echo "no $CONF: copy eval/repos.local.example and point it at your checkouts" >&2; exit 1; }

total_r1=0; total_found=0; total_mrr=0; count=0
while IFS='|' read -r name probes root; do
    case "$name" in ''|'#'*) continue ;; esac
    [ -d "$root" ] || { echo "$name: no checkout at $root"; continue; }
    [ -f "$probes/agent.tsv" ] || { echo "$name: no probes at $probes"; continue; }
    agent=$("$EXE" eval "$probes/agent.tsv" --root "$root" --hits 8 | grep '^  span')
    symbol=$("$EXE" eval "$probes/symbol.tsv" --root "$root" --hits 8 | grep '^  span')
    set -- $agent; r1=$3; found=${11}; mrr=${13}
    set -- $symbol; sym=$3
    printf '%-12s agent R@1 %s  found %s  MRR %s | symbol R@1 %s\n' "$name" "$r1" "$found" "$mrr" "$sym"
    total_r1=$(awk "BEGIN{print $total_r1+$r1}"); total_found=$(awk "BEGIN{print $total_found+$found}")
    total_mrr=$(awk "BEGIN{print $total_mrr+$mrr}"); count=$((count+1))
done < "$CONF"
[ "$count" -gt 0 ] && awk "BEGIN{printf \"%-12s agent R@1 %.3f  found %.3f  MRR %.3f\n\", \"MEAN\", $total_r1/$count, $total_found/$count, $total_mrr/$count}"
