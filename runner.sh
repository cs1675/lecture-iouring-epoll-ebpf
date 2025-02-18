#! /usr/bin/env bash

cargo build --release

echo "" > iouring-server
echo "" > iouring-server-perf
echo "" > rr-server
echo "" > rr-server-perf

for i in $(seq 0 0);
do
	./target/release/lecture-ebpf epoll-server &
	PID=$!
	sleep 1s
	taskset -c 10-70 ./target/release/lecture-ebpf client >> iouring-server &
	perf stat -p $PID &>> iouring-server-perf
	wait $PID

	sleep 5s

	./target/release/lecture-ebpf rr-server &
	PID=$!
	sleep 1s
	taskset -c 10-70 ./target/release/lecture-ebpf client >> rr-server &
	perf stat -p $PID &>> rr-server-perf
	wait $PID
done

