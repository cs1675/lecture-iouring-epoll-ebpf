#! /usr/bin/env bash

OUTPUT=.tmp_output

# Run server
./target/release/app $1 &
SERVER_PID=$!

echo "" > $OUTPUT

# Run client
sleep 1s
taskset -c 10-70 ./target/release/app client &>> $OUTPUT &
CLIENT_PID=$!

# Run perf stat
sleep 1s
echo "Running perf stat on server pid $SERVER_PID" &>> $OUTPUT
perf stat -p $SERVER_PID &>> $OUTPUT &
PERF_PID=$!

# Run syscount
sudo ~/Downloads/bcc/libbpf-tools/syscount -p $SERVER_PID &>> $OUTPUT &
SYSCOUNT_PID=$!

sleep 5s
sudo kill -2 $PERF_PID
sleep 1s
sudo kill -2 $SYSCOUNT_PID

# And wait for everything to end
wait $SERVER_PID
wait $CLIENT_PID

cat .tmp_output
rm .tmp_output
