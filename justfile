track BASE QUOTE CSV_NAME LOG_NAME:
    cargo run --release --bin track -- --url helius_mainnet -b {{BASE}} -q {{QUOTE}} -o data/{{CSV_NAME}}.csv -l logs/{{LOG_NAME}}.log

clear-data:
    rm data/*.csv

clear-logs:
    rm logs/*.log
