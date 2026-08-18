# Helper script for building and bundling Mixtapes for macOS

python_version="3.12"

# Set PATH and DYLD_LIBRARY_PATH for Brew to use Brew Python version (see https://dev.gajim.org/gajim/gajim/-/issues/12365)
DEFAULT_PATH="$PATH"
DEFAULT_DYLD_LIBRARY_PATH="$DYLD_LIBRARY_PATH"
DEFAULT_XDG_DATA_DIRS="$XDG_DATA_DIRS"
if [ "$(uname -m)" == "x86_64" ]
then
	export PATH="/usr/local/bin:/Library/Frameworks/Python.framework/Versions/${python_version}/bin:$PATH"
	export XDG_DATA_DIRS="/usr/local/share:$XDG_DATA_DIRS"
	export DYLD_LIBRARY_PATH="/usr/local/lib:$DYLD_LIBRARY_PATH"
elif [ "$(uname -m)" == "arm64" ]
then
	export PATH="/opt/homebrew/bin:$PATH"
	export XDG_DATA_DIRS="/opt/homebrew/share:$XDG_DATA_DIRS"
	export DYLD_LIBRARY_PATH="/opt/homebrew/lib:$DYLD_LIBRARY_PATH"
fi

function install_brew_dependencies() {
	brew update
	brew install python@${python_version} git
	brew unlink python && brew link python@${python_version}
	brew install gtk4 libadwaita pygobject3 adwaita-icon-theme libsoup@3 gst-python gstreamer dbus
	brew unlink gettext && brew link gettext
	brew unlink libsoup && brew link libsoup@3
	# Reinstall glib via Brew to avoid missing libgobject (see https://github.com/libvips/ruby-vips/issues/284#issuecomment-2040414765)
	brew reinstall glib
	brew unlink glib && brew link glib
}


function build_new_environment() {
	install_brew_dependencies
	python${python_version} -m pip install -r requirements-mac.txt --break-system-packages
}

function create_dmg() {
	python${python_version} -m pip install macholib git+https://github.com/pyinstaller/pyinstaller.git --break-system-packages
	glib-compile-resources --sourcedir=. src/muse.gresource.xml --target=src/muse.gresource
	chmod +x mac/makebundle.py
	./mac/makebundle.py
}

function main()
{
    build_new_environment
    create_dmg
}

main "@"

export DYLD_LIBRARY_PATH="$DEFAULT_DYLD_LIBRARY_PATH"
export XDG_DATA_DIRS="$DEFAULT_XDG_DATA_DIRS"
export PATH="$DEFAULT_PATH"
