#!/bin/sh

docker buildx build -t gabay/canned --platform linux/amd64,linux/arm64 --push .
