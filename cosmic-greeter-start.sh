#!/bin/sh
rm -rf /run/cosmic-greeter/cosmic/com.system76.CosmicSettingsDaemon/v1/* > /dev/null 2>&1
exec cosmic-comp cosmic-greeter > /dev/null 2>&1