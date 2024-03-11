# track BASE/QUOTE pair, dumping books to CSV_NAME and logs to LOG_NAME
track BASE QUOTE CSV_NAME LOG_NAME:
    cargo run --release --bin track -- --url helius_mainnet -b {{BASE}} -q {{QUOTE}} -o data/{{CSV_NAME}}.csv -l logs/{{LOG_NAME}}.log

# print file sizes of data and logs
du:
    @printf "Data:\n" && du -ah data/*.csv | sort -hr | awk '{printf "    %s %s\n", $1, $NF}' | sed 's/data\/\(.*\)/\1/' && printf "\nLogs:\n" && du -ah logs/*.log | sort -hr | awk '{printf "    %s %s\n", $1, $NF}' | sed 's/logs\/\(.*\)/\1/'

clear-data:
    rm data/*.csv

clear-logs:
    rm logs/*.log
