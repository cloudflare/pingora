# Steps to generate nginx upstream ketama hash logs

1. Prepare nginx conf
```
mkdir -p /tmp/nginx-ketama/logs
cp nginx.conf /tmp/nginx-ketama
nginx -t -c nginx.conf -p /tmp/nginx-ketama
```

2. Generate trace
```
./trace.sh
```

3. Collect trace
```
 cp /tmp/nginx-ketama/logs/access.log ./sample-nginx-upstream.csv
```