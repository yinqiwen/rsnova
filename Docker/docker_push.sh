#!/bin/sh

echo "$DOCKER_PASS" | docker login -u "$DOCKER_USER" --password-stdin
docker build --build-arg RSNOVA_VER=${TRAVIS_TAG} -t gsnova/rsnova-server:${TRAVIS_TAG} ./Docker/server
docker push gsnova/rsnova-server:${TRAVIS_TAG}