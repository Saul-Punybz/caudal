{{/*
Chart name, truncated for use in resource names.
*/}}
{{- define "caudal.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "caudal.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "caudal.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "caudal.labels" -}}
helm.sh/chart: {{ include "caudal.chart" . }}
{{ include "caudal.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "caudal.selectorLabels" -}}
app.kubernetes.io/name: {{ include "caudal.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "caudal.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "caudal.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "caudal.image" -}}
{{- $tag := .Values.image.tag | default .Chart.AppVersion -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}

{{/*
[admin] enforcement: caudal-admin refuses to bind a non-loopback address
without a login (crates/caudal-admin's check_exposure), and this chart
always sets `http_bind = "0.0.0.0:8080"` inside the pod. Rather than let
that surface as a crash-looping pod, fail the template up front with a
message that says exactly what to set.
*/}}
{{- define "caudal.checkAdmin" -}}
{{- if .Values.admin.enabled -}}
  {{- if not (or .Values.admin.passwordHash .Values.admin.existingSecret) -}}
    {{ fail "caudal requires an admin login before it will listen on 0.0.0.0 (crates/caudal-admin's check_exposure). Set admin.passwordHash (the output of `printf '%s\\n' '<password>' | caudal hash-password`) or admin.existingSecret (an existing Secret with the hash under admin.existingSecretKey). To run intentionally open instead, set admin.enabled=false and admin.allowUnauthenticated=true." -}}
  {{- end -}}
{{- else if not .Values.admin.allowUnauthenticated -}}
  {{ fail "admin.enabled=false means no login at all: caudal will refuse to start on a non-loopback bind unless you also set admin.allowUnauthenticated=true, acknowledging the server is intentionally open." }}
{{- end -}}
{{- end -}}

{{/*
The Secret name holding the admin password hash: the one this chart
creates, or the operator's existingSecret.
*/}}
{{- define "caudal.adminSecretName" -}}
{{- if .Values.admin.existingSecret -}}
{{- .Values.admin.existingSecret -}}
{{- else -}}
{{- printf "%s-admin" (include "caudal.fullname" .) -}}
{{- end -}}
{{- end -}}

{{- define "caudal.adminSecretKey" -}}
{{- if .Values.admin.existingSecret -}}
{{- .Values.admin.existingSecretKey -}}
{{- else -}}
password-hash
{{- end -}}
{{- end -}}
