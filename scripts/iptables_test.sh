#!/bin/sh

# Create new chain
iptables -t nat -N RSNOVA
iptables -t nat -A PREROUTING -p tcp -j RSNOVA
iptables -t nat -A RSNOVA -p tcp -m set --match-set myipset dst -j REDIRECT --to-ports 48100
iptables -t nat -A RSNOVA -p tcp -j RETURN