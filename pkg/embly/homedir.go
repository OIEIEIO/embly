package embly

import (
	"os"
	"os/user"
	"path/filepath"

	"github.com/pkg/errors"
)

// EmblyDir gets the location of the embly directory
func EmblyDir() (dir string, err error) {
	usr, err := user.Current()
	if err != nil {
		err = errors.WithStack(err)
		return
	}
	dir = filepath.Join(usr.HomeDir, "./.embly/")
	return
}

// CreateHomeDir creates the embly directory in the users home directory
func CreateHomeDir() (err error) {
	dir, err := EmblyDir()
	if err != nil {
		return
	}
	for _, folder := range []string{"./", "./cache", "./nix"} {
		loc := filepath.Join(dir, folder)
		_, err = os.Stat(loc)
		if err != nil {
			err = os.MkdirAll(loc, os.ModePerm)
			if err != nil {
				err = errors.WithStack(err)
				return err
			}
		}
	}
	return nil
}