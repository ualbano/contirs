#!/bin/sh
set -e
echo "${CRON_SCHEDULE:-0 1 * * *} /usr/local/bin/conti" > /etc/crontabs/root
exec crond -f -d 6
