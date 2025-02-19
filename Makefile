
build:
	cargo build --release

iouring: build
	./runner.sh iouring-server

round-robin: build
	./runner.sh rr-server

epoll: build
	./runner.sh epoll-server

