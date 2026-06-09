#!/bin/sh
set -e

if [ "${RUN_ONCE}" = "true" ]; then
    exec /usr/local/bin/conti
fi

echo "${CRON_SCHEDULE:-0 1 * * *} /usr/local/bin/conti" > /etc/crontabs/root
exec crond -f -d 6
