#!/bin/bash

NUM_VALIDATORS="$1"
NUM_SHARDS="$2"

if [ -z "$NUM_VALIDATORS" ] || [ -z "$NUM_SHARDS" ]; then
    echo "USAGE: ./linera-local-run.sh NUM_VALIDATORS NUM_SHARDS" >&2
    exit 1
fi

generate_validators_and_shards() {
    for validator in $(seq 1 $NUM_VALIDATORS); do
        for shard in $(seq 0 $((NUM_SHARDS - 1))); do
            cat << EOF
---
apiVersion: v1
kind: Service
metadata:
  name: server-${validator}-shard-${shard}
  labels:
    app: server-${validator}-shard-${shard}
spec:
  clusterIP: None
  selector:
    app: server-${validator}-shard-${shard}
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: server-${validator}-shard-${shard}
spec:
  selector:
    matchLabels:
      app: server-${validator}-shard-${shard}
  serviceName: server-${validator}-shard-${shard}
  replicas: 1
  template:
    metadata:
      labels:
        app: server-${validator}-shard-${shard}
    spec:
      serviceAccountName: linera-adminserviceaccount
      terminationGracePeriodSeconds: 10
      containers:
        - name: linera-server
          image: us-docker.pkg.dev/linera-io-dev/linera-docker-repo/zefchain-gcp-test:0.1
          imagePullPolicy: Never
          command: ["./linera-server"]
          args:
            [
              "run",
              "--storage",
              "rocksdb:shard_data.db",
              "--server",
              "server_1.json",
              "--shard",
              "${shard}",
              "--genesis",
              "genesis.json",
              "--kube",
            ]
          env:
            - name: MY_POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
            - name: MY_POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
            - name: MY_POD_IP
              valueFrom:
                fieldRef:
                  fieldPath: status.podIP
EOF
        done
        cat << EOF
---
apiVersion: v1
kind: Service
metadata:
  name: validator-${validator}
  labels:
    app: validator-${validator}
spec:
  type: LoadBalancer
  selector:
    app: validator-${validator}
  ports:
    - name: zef
      protocol: TCP
      port: 19100
      targetPort: linera-port
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: validator-${validator}
spec:
  selector:
    matchLabels:
      app: validator-${validator}
  replicas: 1
  template:
    metadata:
      labels:
        app: validator-${validator}
    spec:
      serviceAccountName: linera-adminserviceaccount
      terminationGracePeriodSeconds: 10
      containers:
        - name: linera-proxy
          imagePullPolicy: Never
          image: us-docker.pkg.dev/linera-io-dev/linera-docker-repo/zefchain-gcp-test:0.1
          ports:
            - containerPort: 19100
              name: linera-port
          command: ["./linera-proxy"]
          args: ["server_1.json", "--kube"]
          env:
            - name: MY_POD_NAME
              valueFrom:
                fieldRef:
                  fieldPath: metadata.name
            - name: MY_POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
            - name: MY_POD_IP
              valueFrom:
                fieldRef:
                  fieldPath: status.podIP
EOF
    done
}

cat > linera-local-run.yaml << EOF
apiVersion: v1
kind: ServiceAccount
metadata:
  name: linera-adminserviceaccount
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: linera-adminserviceaccount
subjects:
  - kind: ServiceAccount
    name: linera-adminserviceaccount
    namespace: default
roleRef:
  kind: ClusterRole
  name: cluster-admin
  apiGroup: rbac.authorization.k8s.io
$(generate_validators_and_shards)
EOF
