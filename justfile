# track BASE/QUOTE pair, dumping books to CSV_NAME and logs to LOG_NAME
track-mainnet BASE QUOTE CSV_NAME LOG_NAME:
    RUSTFLAGS="-C target-cpu=native -C opt-level=3" cargo run --release --bin track -- --url helius_mainnet -b {{BASE}} -q {{QUOTE}} -o data/{{CSV_NAME}}.csv -l logs/{{LOG_NAME}}.log

track-devnet BASE QUOTE CSV_NAME LOG_NAME:
    RUSTFLAGS="-C target-cpu=native -C opt-level=3" cargo run --release --bin track -- --url helius_devnet -b {{BASE}} -q {{QUOTE}} -o data/{{CSV_NAME}}-devnet.csv -l logs/{{LOG_NAME}}-devnet.log

trade-mainnet BASE QUOTE:
    RUSTFLAGS="-C target-cpu=native -C opt-level=3" cargo run --release --bin trade -- --url helius_mainnet -b {{BASE}} -q {{QUOTE}}

# print file sizes of data and logs
du:
    @printf "Data:\n" && du -sh data/ | awk '{printf "    (%s total)\n", $1}' && du -ah data/*.csv | sort -hr | awk '{printf "    %s %s\n", $1, $NF}' | sed 's/data\/\(.*\)/\1/' && printf "\nLogs:\n" && du -sh logs/ | awk '{printf "    (%s total)\n", $1}' && du -ah logs/*.log | sort -hr | awk '{printf "    %s %s\n", $1, $NF}' | sed 's/logs\/\(.*\)/\1/'

clear-data:
    rm data/*.csv

clear-logs:
    rm logs/*.log
