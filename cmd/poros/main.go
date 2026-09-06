package main

import (
	"os"

	"github.com/gildrb/poros/internal/poros"
)

var version = "dev"

func main() {
	os.Exit(poros.Run(os.Args[1:], os.Stdout, os.Stderr, version))
}
