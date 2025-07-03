// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <fidl/fuchsia.hardware.pwm/cpp/fidl.h>
#include <lib/ddk/binding.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/metadata.h>

#include <vector>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/gpio/cpp/bind.h>
#include <bind/fuchsia/hardware/gpio/cpp/bind.h>
#include <bind/fuchsia/hardware/pwm/cpp/bind.h>
#include <soc/aml-s905d2/s905d2-pwm.h>

#include "astro-gpios.h"
#include "astro.h"

namespace astro {
namespace fpbus = fuchsia_hardware_platform_bus;

static const std::vector<fpbus::Mmio> pwm_mmios{
    {{
        .base = S905D2_PWM_AB_BASE,
        .length = S905D2_PWM_AB_LENGTH,
    }},
    {{
        .base = S905D2_PWM_CD_BASE,
        .length = S905D2_PWM_AB_LENGTH,
    }},
    {{
        .base = S905D2_PWM_EF_BASE,
        .length = S905D2_PWM_AB_LENGTH,
    }},
    {{
        .base = S905D2_AO_PWM_AB_BASE,
        .length = S905D2_AO_PWM_LENGTH,
    }},
    {{
        .base = S905D2_AO_PWM_CD_BASE,
        .length = S905D2_AO_PWM_LENGTH,
    }},
};

static fpbus::Node pwm_dev = []() {
  fpbus::Node dev = {};
  dev.name() = "pwm";
  dev.vid() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_VID_AMLOGIC;
  dev.pid() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_PID_S905D2;
  dev.did() = bind_fuchsia_amlogic_platform::BIND_PLATFORM_DEV_DID_PWM;
  dev.mmio() = pwm_mmios;
  return dev;
}();

const ddk::BindRule kPwmRules[] = {
    ddk::MakeAcceptBindRule(bind_fuchsia_hardware_pwm::SERVICE,
                            bind_fuchsia_hardware_pwm::SERVICE_ZIRCONTRANSPORT),
    ddk::MakeAcceptBindRule(bind_fuchsia::PWM_ID, static_cast<uint32_t>(S905D2_PWM_E)),
};

const device_bind_prop_t kPwmProperties[] = {
    ddk::MakeProperty(bind_fuchsia_hardware_pwm::SERVICE,
                      bind_fuchsia_hardware_pwm::SERVICE_ZIRCONTRANSPORT),
};

const ddk::BindRule kGpioInitRules[] = {
    ddk::MakeAcceptBindRule(bind_fuchsia::INIT_STEP, bind_fuchsia_gpio::BIND_INIT_STEP_GPIO),
};

const device_bind_prop_t kGpioInitProperties[] = {
    ddk::MakeProperty(bind_fuchsia::INIT_STEP, bind_fuchsia_gpio::BIND_INIT_STEP_GPIO),
};

const ddk::BindRule kGpioBtRules[] = {
    ddk::MakeAcceptBindRule(bind_fuchsia_hardware_gpio::SERVICE,
                            bind_fuchsia_hardware_gpio::SERVICE_ZIRCONTRANSPORT),
    ddk::MakeAcceptBindRule(bind_fuchsia::GPIO_PIN, static_cast<uint32_t>(GPIO_SOC_BT_REG_ON)),
};

const device_bind_prop_t kGpioBtProperties[] = {
    ddk::MakeProperty(bind_fuchsia_hardware_gpio::SERVICE,
                      bind_fuchsia_hardware_gpio::SERVICE_ZIRCONTRANSPORT),
    ddk::MakeProperty(bind_fuchsia_gpio::FUNCTION, bind_fuchsia_gpio::FUNCTION_BT_REG_ON),
};

zx_status_t Astro::PwmInit() {
  gpio_init_steps_.push_back(GpioFunction(GPIO_SOC_WIFI_LPO_32k768, S905D2_PWM_E_FN));

  /* PWM_AO_B used by bootloader to control PP800_EE rail. The init flag is set
  to false to prevent access to that channel as the configuration set by the
  bootloader must be preserved for proper SoC operation. */
  fuchsia_hardware_pwm::PwmChannelsMetadata metadata = {{{{
      {{.id = S905D2_PWM_A}},
      {{.id = S905D2_PWM_B}},
      {{.id = S905D2_PWM_C}},
      {{.id = S905D2_PWM_D}},
      {{.id = S905D2_PWM_E}},
      {{.id = S905D2_PWM_F}},
      {{.id = S905D2_PWM_AO_A}},
      {{.id = S905D2_PWM_AO_B, .skip_init = true}},
      {{.id = S905D2_PWM_AO_C}},
      {{.id = S905D2_PWM_AO_D}},
  }}}};

  fit::result persisted_metadata = fidl::Persist(metadata);
  if (persisted_metadata.is_error()) {
    zxlogf(ERROR, "Failed to persist pwm channels metadata: %s",
           persisted_metadata.error_value().FormatDescription().c_str());
    return persisted_metadata.error_value().status();
  }

  const std::vector<fpbus::Metadata> pwm_metadata{
      {{
          .id = fuchsia_hardware_pwm::PwmChannelsMetadata::kSerializableName,
          .data = std::move(persisted_metadata.value()),
      }},
  };
  pwm_dev.metadata() = pwm_metadata;

  fidl::Arena<> fidl_arena;
  fdf::Arena arena('PWM_');
  auto result = pbus_.buffer(arena)->NodeAdd(fidl::ToWire(fidl_arena, pwm_dev));
  if (!result.ok()) {
    zxlogf(ERROR, "%s: NodeAdd Pwm(pwm_dev) request failed: %s", __func__,
           result.FormatDescription().data());
    return result.status();
  }
  if (result->is_error()) {
    zxlogf(ERROR, "%s: NodeAdd Pwm(pwm_dev) failed: %s", __func__,
           zx_status_get_string(result->error_value()));
    return result->error_value();
  }

  zx_status_t status =
      DdkAddCompositeNodeSpec("pwm_init", ddk::CompositeNodeSpec(kPwmRules, kPwmProperties)
                                              .AddParentSpec(kGpioInitRules, kGpioInitProperties)
                                              .AddParentSpec(kGpioBtRules, kGpioBtProperties));
  if (status != ZX_OK) {
    zxlogf(ERROR, "DdkAddCompositeNodeSpec failed: %s", zx_status_get_string(status));
    return status;
  }

  return ZX_OK;
}

}  // namespace astro
