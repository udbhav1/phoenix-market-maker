# track BASE/QUOTE pair, dumping books to CSV_NAME and logs to LOG_NAME
track BASE QUOTE CSV_NAME LOG_NAME:
    RUSTFLAGS="-C target-cpu=native -C opt-level=3" cargo run --release --bin track -- -b {{BASE}} -q {{QUOTE}} -o data/{{CSV_NAME}}.csv -l logs/{{LOG_NAME}}.log

# track BASE/QUOTE pair and oracle, dumping books to CSV_NAME and logs to LOG_NAME
track-oracle BASE QUOTE CSV_NAME LOG_NAME:
    RUSTFLAGS="-C target-cpu=native -C opt-level=3" cargo run --release --bin track -- -b {{BASE}} -q {{QUOTE}} --oracle -o data/{{CSV_NAME}}.csv -l logs/{{LOG_NAME}}.log

# setup and quote BASE/QUOTE pair, dumping fills to CSV_NAME and logs to LOG_NAME
trade BASE QUOTE CSV_NAME LOG_NAME KEYPAIR:
    RUST_BACKTRACE=1 RUSTFLAGS="-C target-cpu=native -C opt-level=3" cargo run --release --bin trade -- -b {{BASE}} -q {{QUOTE}} -o trades/{{CSV_NAME}}.csv -l logs/{{LOG_NAME}}.log -k {{KEYPAIR}}

# stalk ADDRESS on BASE/QUOTE pair, dumping fills to CSV_NAME and logs to LOG_NAME
stalk BASE QUOTE CSV_NAME LOG_NAME ADDRESS:
    RUST_BACKTRACE=1 RUSTFLAGS="-C target-cpu=native -C opt-level=3" cargo run --release --bin stalk -- -b {{BASE}} -q {{QUOTE}} -o trades/{{CSV_NAME}}.csv -l logs/{{LOG_NAME}}.log -a {{ADDRESS}}

# open track dashboard
track-ui:
    voila notebooks/book-dashboard.ipynb

# open fills dashboard
trade-ui:
    voila notebooks/fill-dashboard.ipynb

# print file sizes of data/, logs/, trades/
du:
    @du -sh trades/ | awk '{printf "Trades (%s total):\n", $1}' && du -ah trades/*.csv | sort -hr | awk '{printf "    %s %s\n", $1, $NF}' | sed 's/trades\/\(.*\)/\1/' && du -sh data/ | awk '{printf "\nData (%s total):\n", $1}' && du -ah data/*.csv | sort -hr | awk '{printf "    %s %s\n", $1, $NF}' | sed 's/data\/\(.*\)/\1/' && du -sh logs/ | awk '{printf "\nLogs (%s total):\n", $1}' && du -ah logs/*.log | sort -hr | awk '{printf "    %s %s\n", $1, $NF}' | sed 's/logs\/\(.*\)/\1/'

watch URL MARKET TRADER:
    watch -n1 "phoenix-cli get-open-orders {{MARKET}} -t {{TRADER}} --url {{URL}}"

balance KEYPAIR:
    @spl-token accounts --url m --owner {{KEYPAIR}} | tail -n +3
    @solana balance --url m --keypair {{KEYPAIR}}

clear-trades:
    rm trades/*.csv

clear-data:
    rm data/*.csv

clear-logs:
    rm logs/*.log
