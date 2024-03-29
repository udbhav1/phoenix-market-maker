import asyncio
import time
import websockets  # type: ignore
from dotenv import load_dotenv  # type: ignore
import os
import statistics

load_dotenv()

api_key = os.getenv("HELIUS_API_KEY")
uris = [
    f"wss://mainnet.helius-rpc.com/?api-key={api_key}",
    f"wss://atlas-mainnet.helius-rpc.com/?api-key={api_key}",
]
N = 10


async def measure_ping(uri, n):
    ping_times = []
    async with websockets.connect(uri) as websocket:
        for i in range(n):
            start_time = time.time()
            await websocket.send("ping")
            # Wait for the response
            response = await websocket.recv()
            end_time = time.time()

            ping_time = (end_time - start_time) * 1000
            ping_times.append(ping_time)
            print(f"Round trip {i+1}/{n}: {ping_time:.2f} ms")

        # Calculate min, median, max
        min_ping = min(ping_times)
        median_ping = statistics.median(ping_times)
        max_ping = max(ping_times)

        return min_ping, median_ping, max_ping


for uri in uris:
    print(f"Pinging {uri}")
    min_ping, median_ping, max_ping = asyncio.get_event_loop().run_until_complete(
        measure_ping(uri, N)
    )
    print("--------------------")
    print(f"Min Ping:    {min_ping:.2f} ms")
    print(f"Median Ping: {median_ping:.2f} ms")
    print(f"Max Ping:    {max_ping:.2f} ms")
    print()
