// Compiled only by the offline parity overlay, never into upstream files.
package config

import "gopkg.in/yaml.v3"

var ParityRejected func([]byte)

func parityUnmarshal(data []byte, out interface{}) error {
	err := yaml.Unmarshal(data, out)
	if err != nil && ParityRejected != nil {
		ParityRejected(data)
	}
	return err
}
