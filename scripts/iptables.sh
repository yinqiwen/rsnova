#!/bin/sh

# Create new chain
iptables -t nat -N RSNOVA
if [ $? -eq 0 ]
then
  iptables -t nat -A PREROUTING -p tcp -j RSNOVA
else
  echo "RSNOVA exists"
fi
iptables -t nat -F RSNOVA

# Ignore your shadowsocks server's addresses
# It's very IMPORTANT, just be careful.
iptables -t nat -A RSNOVA -d 43.155.93.10 -j RETURN


iptables -t nat -A RSNOVA -d 0.0.0.0/8 -j RETURN
iptables -t nat -A RSNOVA -d 10.0.0.0/8 -j RETURN
iptables -t nat -A RSNOVA -d 127.0.0.0/8 -j RETURN
iptables -t nat -A RSNOVA -d 169.254.0.0/16 -j RETURN
iptables -t nat -A RSNOVA -d 172.16.0.0/12 -j RETURN
iptables -t nat -A RSNOVA -d 192.168.0.0/16 -j RETURN
iptables -t nat -A RSNOVA -d 224.0.0.0/4 -j RETURN
iptables -t nat -A RSNOVA -d 240.0.0.0/4 -j RETURN
# iptables -t nat -A RSNOVA -p tcp -m set --match-set chnip dst -j RETURN
iptables -t nat -A RSNOVA -p tcp -m set --match-set gfwip dst -j REDIRECT --to-ports 48100
iptables -t nat -A RSNOVA -p tcp -m set --match-set gfwip6 dst -j REDIRECT --to-ports 48100
# iptables -t nat -A RSNOVA -p tcp -j REDIRECT --to-ports 48100
# iptables -t nat -A RSNOVA -p tcp -m set --match-set gfwip dst -j REDIRECT --to-ports 48100

iptables -t nat -A RSNOVA -p tcp -j RETURN