CERT_PASSWORD?=password
CA_SUBJECT="/C=IE/ST=Leinster/L=Dublin/O=None/CN=localhost"
SERVER_SUBJECT="/C=IE/ST=Donegal/L=Ballintra/O=None/CN=localhost"
DAYS="800"

default: clean server.pfx root-ca.pem

%.key:
	# Generate RSA Key for CA
	openssl genrsa -out $@ 2048

%-ca.pem: %-ca.key
	# CA Cert out of CA Key to represent CA
	openssl req -new -x509 -nodes -days ${DAYS} -subj "${CA_SUBJECT}" -key $< -config ext-sign -extensions ca_profile -out $@

%.csr: %.key
	# Server signing request pem and server key
	openssl req -new -subj "${SERVER_SUBJECT}" -nodes -days ${DAYS} -key $<  -out $*.csr

%.pem: %.csr root-ca.pem root-ca.key
	# Server certificate, what will be shown to https clients, it needs to be signed by the CA and requires a signing request
	openssl x509 -req -sha256 -days ${DAYS} -set_serial 01 -in $*.csr -CA root-ca.pem -CAkey root-ca.key -out $@ -extfile ext-san-config

%.pfx: %.pem %.key
	# Runtime private information for the https server, it has its key, its certificate and the ca certificate, this is the minimum to run a server
	echo "${CERT_PASSWORD}" | openssl pkcs12 -export -inkey $*.key -in $*.pem -passout stdin -out $@

clean:
	rm -f *.pem *.key *.pfx

verify: server-cert.pem server.pfx
	openssl verify -CAfile ca-cert.pem server-cert.pem
	openssl pkcs12 -info -in server.pfx -passin pass:password -passout pass:password -noout
	security verify-cert -c server-cert.pem -r ca-cert.pem
	security verify-cert -c server-cert.pem -r ca-cert.pem -p ssl
	openssl pkcs12 -info -in server.pfx -passin pass:${CERT_PASSWORD} -passout pass:${CERT_PASSWORD} -noout

show_certs:
	openssl x509 -text -noout -in ca-cert.pem
	openssl req -text -noout -verify -in server-req.pem
	openssl x509 -text -noout -in server-cert.pem
