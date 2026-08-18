# [CERTER]
# APISIX Letsencrypt certificate generator Dockerfile
# Menke 2023
FROM alpine:3.17

# Default env variables
ENV TTL=60 \
  CERT_TYPE=staging \
  PORT=80 \
  CURL_DEBUG=false \
  CURL_QUIET=true \
  CURL_IGNORESSL=true \
  DELETE_OTHER_CERTS=true \
  AGREE_TOS=true



RUN apk update
RUN apk add bash certbot curl jq yq

ADD scripts/certer.sh /certer.sh
ADD scripts/provision-routes.sh /provision-routes.sh

RUN ["chmod", "+x", "/certer.sh"]
RUN ["chmod", "+x", "/provision-routes.sh"]

EXPOSE 80
CMD ["/certer.sh"]
