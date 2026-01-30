#!/bin/sh
ipset create chnip hash:ip
ipset create chnip6 hash:ip
ipset create gfwip hash:ip
ipset create gfwip6 hash:ip
ipset create chnroute hash:ip
ipset create chnroute6 hash:ip