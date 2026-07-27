package main

import (
	"net/http"
	"testing"
	"time"
)

func TestSetTimingHeaders(t *testing.T) {
	headers := make(http.Header)
	timing := roundTripTiming{
		connectionReused: true,
		reconnected:      false,
		dial: dialTiming{
			networkDial:  17 * time.Millisecond,
			tlsHandshake: 23 * time.Millisecond,
		},
		upstreamHeaders: 41 * time.Millisecond,
	}

	setTimingHeaders(headers, 42, timing)

	checks := map[string]string{
		headerTimingRequestID:       "42",
		headerTimingConnectionReuse: "true",
		headerTimingReconnected:     "false",
		headerTimingNetworkDialUS:   "17000",
		headerTimingTLSHandshakeUS:  "23000",
		headerTimingUpstreamHeadUS:  "41000",
	}
	for name, want := range checks {
		if got := headers.Get(name); got != want {
			t.Fatalf("%s = %q, want %q", name, got, want)
		}
	}
}
